use cs_core::Caption;
use std::time::{Duration, Instant};

struct TimedCaption {
    caption: Caption,
    updated_at: Instant,
}

pub(super) struct OverlayMessages {
    entries: Vec<TimedCaption>,
    lifetime: Duration,
}

impl OverlayMessages {
    pub(super) fn new(lifetime: Duration) -> Self {
        Self {
            entries: Vec::with_capacity(2),
            lifetime,
        }
    }

    pub(super) fn push(&mut self, caption: &Caption, now: Instant) {
        self.expire(now);
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.caption.id == caption.id)
        {
            entry.caption.clone_from(caption);
            entry.updated_at = now;
        } else {
            if self.entries.len() == 2 {
                self.entries.remove(0);
            }
            self.entries.push(TimedCaption {
                caption: caption.clone(),
                updated_at: now,
            });
        }
    }

    pub(super) fn clear(&mut self) {
        self.entries.clear();
    }

    pub(super) fn set_lifetime(&mut self, lifetime: Duration, now: Instant) {
        // A longer timeout must not bring back messages that already expired.
        self.expire(now);
        self.lifetime = lifetime;
        self.expire(now);
    }

    pub(super) fn expire(&mut self, now: Instant) -> bool {
        let previous = self.entries.len();
        self.entries
            .retain(|entry| entry.updated_at + self.lifetime > now);
        self.entries.len() != previous
    }

    pub(super) fn next_expiry(&self) -> Option<Instant> {
        self.entries
            .iter()
            .map(|entry| entry.updated_at + self.lifetime)
            .min()
    }

    pub(super) fn captions(&self, now: Instant) -> impl Iterator<Item = &Caption> {
        self.entries
            .iter()
            .filter(move |entry| entry.updated_at + self.lifetime > now)
            .map(|entry| &entry.caption)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caption(id: u64, text: &str) -> Caption {
        Caption {
            id,
            start_ms: 0,
            end_ms: 100,
            channel: "voice".into(),
            speakers: Vec::new(),
            original: text.into(),
            translated: text.into(),
            translation_pending: false,
            translation_error: None,
            language: Some("en".into()),
            is_final: true,
        }
    }

    #[test]
    fn updates_restart_expiry_and_deadline_is_exclusive() {
        let now = Instant::now();
        let mut messages = OverlayMessages::new(Duration::from_secs(8));
        messages.push(&caption(1, "Go B"), now);
        messages.push(&caption(1, "Don't go B"), now + Duration::from_secs(7));
        assert!(!messages.expire(now + Duration::from_secs(8)));
        assert_eq!(
            messages
                .captions(now + Duration::from_secs(14))
                .next()
                .unwrap()
                .translated,
            "Don't go B"
        );
        assert_eq!(messages.next_expiry(), Some(now + Duration::from_secs(15)));
        assert!(messages.expire(now + Duration::from_secs(15)));
        assert!(
            messages
                .captions(now + Duration::from_secs(15))
                .next()
                .is_none()
        );
        assert_eq!(messages.next_expiry(), None);
    }

    #[test]
    fn captions_expire_independently_and_only_latest_two_are_kept() {
        let now = Instant::now();
        let mut messages = OverlayMessages::new(Duration::from_secs(8));
        for id in 1..=3 {
            messages.push(&caption(id, "Go B"), now + Duration::from_secs(id - 1));
        }
        assert_eq!(
            messages
                .captions(now + Duration::from_secs(2))
                .map(|caption| caption.id)
                .collect::<Vec<_>>(),
            [2, 3]
        );
        assert!(messages.expire(now + Duration::from_secs(9)));
        assert_eq!(
            messages
                .captions(now + Duration::from_secs(9))
                .map(|caption| caption.id)
                .collect::<Vec<_>>(),
            [3]
        );
        assert!(messages.expire(now + Duration::from_secs(10)));
        assert_eq!(messages.next_expiry(), None);
    }

    #[test]
    fn duration_changes_apply_without_reviving_expired_messages() {
        let now = Instant::now();
        let mut messages = OverlayMessages::new(Duration::from_secs(8));
        messages.push(&caption(1, "Go B"), now);
        messages.push(&caption(2, "Go A"), now + Duration::from_secs(4));
        messages.set_lifetime(Duration::from_secs(20), now + Duration::from_secs(8));
        assert_eq!(
            messages
                .captions(now + Duration::from_secs(8))
                .map(|caption| caption.id)
                .collect::<Vec<_>>(),
            [2]
        );
        messages.set_lifetime(Duration::from_secs(2), now + Duration::from_secs(8));
        assert_eq!(messages.next_expiry(), None);
        messages.push(&caption(3, "Wait"), now + Duration::from_secs(9));
        messages.clear();
        assert_eq!(messages.next_expiry(), None);
    }
}
