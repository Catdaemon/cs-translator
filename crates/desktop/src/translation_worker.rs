use anyhow::{Result, bail};
use crossbeam_channel::{Receiver, bounded};
use cs_core::AppConfig;
use cs_inference::Translator;
use std::{
    collections::VecDeque,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

#[derive(Clone, Debug)]
pub(super) struct Job {
    pub id: u64,
    pub text: String,
    pub source: String,
    pub target: String,
    pub policy: u64,
    pub final_result: bool,
}
pub(super) struct TranslationResult {
    pub job: Job,
    pub result: Result<String>,
}
pub(super) struct Submission {
    pub accepted: bool,
    pub evicted: Option<Job>,
}
struct Queue {
    jobs: VecDeque<Job>,
    closed: bool,
}
pub(super) struct TranslationWorker {
    queue: Arc<(Mutex<Queue>, Condvar)>,
    pub results: Receiver<TranslationResult>,
    handle: Option<thread::JoinHandle<()>>,
    stop: Arc<AtomicBool>,
}
impl TranslationWorker {
    pub fn start(config: &AppConfig, stop: Arc<AtomicBool>) -> Result<Self> {
        let queue = Arc::new((
            Mutex::new(Queue {
                jobs: VecDeque::new(),
                closed: false,
            }),
            Condvar::new(),
        ));
        let (ready_tx, ready_rx) = bounded(1);
        let (result_tx, results) = bounded(64);
        let work = queue.clone();
        let stopped = stop.clone();
        let path = config.translation_model.clone();
        let threads = config.threads;
        let beam = config.translation_beam_size;
        let handle = thread::spawn(move || {
            // Native thread-local limits must be established on the thread doing inference.
            let mut translator = match Translator::load(&path, threads, beam) {
                Ok(value) => {
                    let _ = ready_tx.send(Ok(()));
                    value
                }
                Err(error) => {
                    let _ = ready_tx.send(Err(format!("{error:#}")));
                    return;
                }
            };
            loop {
                let job = {
                    let (lock, wake) = &*work;
                    let mut queue = lock.lock().unwrap();
                    while queue.jobs.is_empty() && !queue.closed && !stopped.load(Ordering::Relaxed)
                    {
                        queue = wake
                            .wait_timeout(queue, Duration::from_millis(25))
                            .unwrap()
                            .0;
                    }
                    if queue.closed || stopped.load(Ordering::Relaxed) {
                        break;
                    }
                    queue.jobs.pop_front().unwrap()
                };
                let result = translator.translate(&job.text, &job.source, &job.target);
                let mut value = TranslationResult { job, result };
                loop {
                    match result_tx.send_timeout(value, Duration::from_millis(25)) {
                        Ok(()) => break,
                        Err(crossbeam_channel::SendTimeoutError::Timeout(v))
                            if !stopped.load(Ordering::Relaxed) =>
                        {
                            value = v
                        }
                        Err(_) => return,
                    }
                }
            }
        });
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                queue,
                results,
                handle: Some(handle),
                stop,
            }),
            other => {
                let _ = handle.join();
                bail!("Load translation worker: {:?}", other);
            }
        }
    }
    pub fn submit(&self, job: Job) -> Result<Submission> {
        let (lock, wake) = &*self.queue;
        let mut queue = lock.lock().unwrap();
        let submission = enqueue(&mut queue.jobs, job)?;
        if submission.accepted {
            wake.notify_one();
        }
        Ok(submission)
    }
    pub fn is_finished(&self) -> bool {
        self.handle
            .as_ref()
            .is_none_or(|handle| handle.is_finished())
    }
}
fn enqueue(jobs: &mut VecDeque<Job>, job: Job) -> Result<Submission> {
    // A revision supersedes only this caption: unrelated queued finals keep FIFO order.
    jobs.retain(|old| old.id != job.id);
    let mut evicted = None;
    if jobs.len() >= 256 {
        if !job.final_result {
            return Ok(Submission {
                accepted: false,
                evicted: None,
            });
        }
        if let Some(index) = jobs.iter().position(|old| !old.final_result) {
            evicted = jobs.remove(index);
        } else {
            return Ok(Submission {
                accepted: false,
                evicted: None,
            });
        }
    }
    jobs.push_back(job);
    Ok(Submission {
        accepted: true,
        evicted,
    })
}
impl Drop for TranslationWorker {
    fn drop(&mut self) {
        self.queue.0.lock().unwrap().closed = true;
        self.queue.1.notify_all();
        // Result delivery must not obstruct shutdown after the consumer exits.
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn job(id: u64, text: &str, final_result: bool) -> Job {
        Job {
            id,
            text: text.into(),
            source: "en".into(),
            target: "de".into(),
            policy: 0,
            final_result,
        }
    }
    #[test]
    fn revisions_coalesce_without_dropping_queued_finals() {
        let mut jobs = VecDeque::new();
        enqueue(&mut jobs, job(1, "old", false)).unwrap();
        enqueue(&mut jobs, job(2, "final", true)).unwrap();
        enqueue(&mut jobs, job(1, "new", false)).unwrap();
        enqueue(&mut jobs, job(1, "final", true)).unwrap();
        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[0].id, 2);
        assert!(jobs.iter().all(|job| job.final_result));
    }
    #[test]
    fn saturated_finals_defer_without_losing_any() {
        let mut jobs = VecDeque::new();
        for id in 0..256 {
            enqueue(&mut jobs, job(id, "final", true)).unwrap();
        }
        assert!(
            !enqueue(&mut jobs, job(300, "final", true))
                .unwrap()
                .accepted
        );
        assert_eq!(jobs.len(), 256);
    }
    #[test]
    fn policy_change_replaces_obsolete_final_backlog() {
        let mut jobs = VecDeque::new();
        for id in 0..129 {
            enqueue(&mut jobs, job(id, "old target", true)).unwrap();
        }
        for id in 0..129 {
            let mut replacement = job(id, "new target", true);
            replacement.policy = 1;
            replacement.target = "fr".into();
            enqueue(&mut jobs, replacement).unwrap();
        }
        assert_eq!(jobs.len(), 129);
        assert!(jobs.iter().all(|job| job.policy == 1 && job.target == "fr"));
    }
    #[test]
    fn player_source_revision_keeps_unrelated_final_work() {
        let mut jobs = VecDeque::new();
        enqueue(&mut jobs, job(1, "first", true)).unwrap();
        enqueue(&mut jobs, job(2, "unrelated", true)).unwrap();
        let mut revised = job(1, "first", true);
        revised.policy = 7;
        revised.source = "ru".into();
        enqueue(&mut jobs, revised).unwrap();
        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[0].id, 2);
        assert_eq!(jobs[0].policy, 0);
        assert_eq!(jobs[1].source, "ru");
        assert_eq!(jobs[1].policy, 7);
    }

    #[test]
    fn deferred_source_revisions_drain_three_hundred_finals_in_order() {
        let mut queue = VecDeque::new();
        enqueue(&mut queue, job(999, "unrelated", true)).unwrap();
        let mut deferred: VecDeque<_> = (0..300)
            .map(|id| {
                let mut value = job(id, "retained text", true);
                value.source = "ru".into();
                value.policy = 8;
                value
            })
            .collect();
        let mut consumed = Vec::new();
        while !deferred.is_empty() || !queue.is_empty() {
            while let Some(next) = deferred.front() {
                if !enqueue(&mut queue, next.clone()).unwrap().accepted {
                    break;
                }
                deferred.pop_front();
            }
            assert!(queue.len() <= 256);
            let next = queue
                .pop_front()
                .expect("backpressure still permits progress");
            consumed.push(next);
        }
        assert_eq!(consumed.len(), 301);
        assert_eq!(consumed[0].id, 999);
        assert_eq!(consumed[0].source, "en");
        for (id, value) in consumed[1..].iter().enumerate() {
            assert_eq!(value.id, id as u64);
            assert_eq!(value.source, "ru");
            assert_eq!(value.text, "retained text");
            assert_eq!(value.policy, 8);
        }
    }

    #[test]
    fn saturated_partial_is_retryable_after_backlog_drains() {
        let mut jobs = (0..256).map(|id| job(id, "final", true)).collect();
        let rejected = enqueue(&mut jobs, job(300, "Go B", false)).unwrap();
        assert!(!rejected.accepted);
        jobs.pop_front();
        let retried = enqueue(&mut jobs, job(300, "Go B", false)).unwrap();
        assert!(retried.accepted);
        assert_eq!(jobs.back().unwrap().text, "Go B");
    }
    #[test]
    fn final_admission_identifies_partial_that_needs_retrying() {
        let mut jobs = (0..255).map(|id| job(id, "final", true)).collect();
        enqueue(&mut jobs, job(300, "Go B", false)).unwrap();
        let final_job = enqueue(&mut jobs, job(301, "Stop", true)).unwrap();
        let evicted = final_job
            .evicted
            .expect("Superseded partial must remain retryable");
        assert_eq!(evicted.id, 300);
        assert_eq!(evicted.text, "Go B");
        assert!(final_job.accepted);
        jobs.pop_front();
        assert!(enqueue(&mut jobs, evicted).unwrap().accepted);
        assert!(jobs.iter().any(|job| job.id == 301 && job.final_result));
        assert!(jobs.iter().any(|job| job.id == 300 && !job.final_result));
    }
}
