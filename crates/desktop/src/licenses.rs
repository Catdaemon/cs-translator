use anyhow::{Context as _, Result, bail};
use gpui_kit::component::{
    Disableable, Selectable, TitleBar,
    button::{Button, ButtonVariants},
};
use gpui_kit::{prelude::*, *};
use serde::Deserialize;
use std::{
    collections::BTreeSet,
    fs,
    path::{Component, Path, PathBuf},
    sync::Arc,
};

#[derive(Deserialize)]
struct Dependency {
    name: String,
    version: String,
    license: Option<String>,
    repository: Option<String>,
    notices: Vec<String>,
}

#[derive(Deserialize)]
struct Source {
    file: String,
    url: String,
}

struct Entry {
    title: SharedString,
    description: String,
    files: Vec<(String, PathBuf)>,
}

// All paths come from the packaged catalog, but must still stay inside its tree.
fn relative_path(root: &Path, value: &str) -> Result<PathBuf> {
    let path = Path::new(value);
    if path
        .components()
        .any(|part| !matches!(part, Component::Normal(_)))
    {
        bail!("Invalid packaged notice path: {value}");
    }
    Ok(root.join(path))
}

fn catalog(asset_root: PathBuf) -> Result<Vec<Arc<Entry>>> {
    let root = asset_root.join("licenses");
    let dependencies: Vec<Dependency> =
        serde_json::from_slice(&fs::read(root.join("dependencies.json")).context(
            "Cannot read licenses/dependencies.json. Use a complete extracted release package.",
        )?)
        .context("Invalid packaged Cargo license catalog")?;
    let sources: Vec<Source> = serde_json::from_slice(
        &fs::read(root.join("sources.json")).context("Cannot read licenses/sources.json")?,
    )
    .context("Invalid packaged native/model license catalog")?;
    let mut entries = Vec::with_capacity(dependencies.len() + sources.len() + 2);
    for dependency in dependencies {
        let title = format!("{} {}", dependency.name, dependency.version);
        let directory = relative_path(
            &root.join("dependencies"),
            &format!("{}-{}", dependency.name, dependency.version),
        )?;
        let mut files = Vec::new();
        let mut seen = BTreeSet::new();
        for notice in dependency.notices {
            if seen.insert(notice.clone()) {
                files.push((notice.clone(), relative_path(&directory, &notice)?));
            }
        }
        if let Some(expression) = &dependency.license {
            // Match the packaging script's SPDX tokenization, including exceptions.
            let identifiers: BTreeSet<_> = expression
                .split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '+' | '-')))
                .filter(|id| !id.is_empty() && !matches!(*id, "AND" | "OR" | "WITH"))
                .collect();
            for identifier in identifiers {
                files.push((
                    format!("SPDX: {identifier}"),
                    relative_path(&root.join("spdx"), &format!("{identifier}.txt"))?,
                ));
            }
        }
        if files.is_empty() {
            bail!("No license texts declared for {title}");
        }
        entries.push(Arc::new(Entry {
            title: title.into(),
            description: format!(
                "Cargo dependency\nDeclared license: {}\nRepository: {}",
                dependency
                    .license
                    .as_deref()
                    .unwrap_or("See declared license file"),
                dependency
                    .repository
                    .as_deref()
                    .unwrap_or("Not specified in Cargo metadata")
            ),
            files,
        }));
    }
    // Include every shipped top-level notice, not just the pinned source records:
    // artwork and any supplementary notices must remain discoverable as well.
    let mut root_files = BTreeSet::new();
    for source in &sources {
        root_files.insert(source.file.clone());
    }
    for item in fs::read_dir(&root).context("Cannot enumerate packaged supplementary notices")? {
        let item = item?;
        if item.file_type()?.is_file() {
            let name = item
                .file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("Non-Unicode packaged license filename"))?;
            if name != "dependencies.json" && name != "sources.json" {
                root_files.insert(name);
            }
        }
    }
    for name in root_files {
        let source = sources.iter().find(|source| source.file == name);
        entries.push(Arc::new(Entry {
            title: name.clone().into(),
            description: source
                .map(|source| {
                    format!(
                        "Native / model / supplementary notice\nSource: {}",
                        source.url
                    )
                })
                .unwrap_or_else(|| "Packaged artwork / supplementary notice".into()),
            files: vec![(name.clone(), relative_path(&root, &name)?)],
        }));
    }
    entries.push(Arc::new(Entry {
        title: "Third-party attribution and model notices".into(),
        description: "Package-wide attribution, model terms and supplementary notices".into(),
        files: vec![(
            "THIRD-PARTY-NOTICES.txt".into(),
            asset_root.join("THIRD-PARTY-NOTICES.txt"),
        )],
    }));
    entries.sort_by(|a, b| a.title.cmp(&b.title));
    Ok(entries)
}

fn load_text(entry: &Entry) -> SharedString {
    let mut text = format!("{}\n{}\n", entry.title, entry.description);
    // Do not discard successfully read notices when another file is missing.
    // Errors appear inline at the exact missing notice and are copyable too.
    for (label, path) in &entry.files {
        text.push_str(&format!(
            "\n────────────────────────────────\n{label}\n────────────────────────────────\n\n"
        ));
        match fs::read(path).with_context(|| format!("Cannot read {}", path.display())) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(contents) => text.push_str(&contents),
                Err(error) => text.push_str(&format!("ERROR: Notice is not UTF-8: {error}")),
            },
            Err(error) => text.push_str(&format!("ERROR: {error:#}")),
        }
        text.push('\n');
    }
    text.into()
}

pub(super) struct LicenseBrowser {
    entries: Vec<Arc<Entry>>,
    selected: Option<usize>,
    generation: u64,
    status: SharedString,
    text: Option<SharedString>,
}

impl LicenseBrowser {
    fn select(&mut self, index: usize, cx: &mut Context<Self>) {
        let Some(entry) = self.entries.get(index).cloned() else {
            return;
        };
        self.selected = Some(index);
        self.generation += 1;
        let generation = self.generation;
        self.text = None;
        self.status = "Loading full license and notice texts…".into();
        let work = cx
            .background_executor()
            .spawn(async move { load_text(&entry) });
        cx.spawn(async move |view, cx| {
            let result = work.await;
            let _ = view.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.text = Some(result);
                this.status = "".into();
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }
}

pub(super) fn open(asset_root: PathBuf, cx: &mut App) -> Result<AnyWindowHandle> {
    let (handle, _) = gpui_kit::open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                None,
                size(px(1100.), px(800.)),
                cx,
            ))),
            window_min_size: Some(size(px(720.), px(480.))),
            titlebar: Some(TitlebarOptions {
                title: Some("Open-source licenses".into()),
                ..TitleBar::title_bar_options()
            }),
            ..TitleBar::window_options()
        },
        cx,
        move |_, cx| {
            cx.new(|cx| {
                let work = cx
                    .background_executor()
                    .spawn(async move { catalog(asset_root) });
                cx.spawn(async move |view, cx| {
                    let result = work.await;
                    let _ = view.update(cx, |this: &mut LicenseBrowser, cx| {
                        match result {
                            Ok(entries) => {
                                this.entries = entries;
                                if this.entries.is_empty() {
                                    this.status = "The packaged license catalog is empty.".into();
                                } else {
                                    this.select(0, cx);
                                }
                            }
                            Err(error) => {
                                this.status =
                                    format!("Cannot open packaged licenses: {error:#}").into()
                            }
                        }
                        cx.notify();
                    });
                })
                .detach();
                LicenseBrowser {
                    entries: Vec::new(),
                    selected: None,
                    generation: 0,
                    status: "Loading packaged license catalog…".into(),
                    text: None,
                }
            })
        },
    )?;
    Ok(handle)
}

impl Render for LicenseBrowser {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let list = uniform_list(
            "license-components",
            self.entries.len(),
            cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                range
                    .map(|index| {
                        let entry = &this.entries[index];
                        Button::new(("license-component", index))
                            .ghost()
                            .w_full()
                            .h(px(36.))
                            .when(this.selected == Some(index), |button| button.primary())
                            .selected(this.selected == Some(index))
                            .accessibility_label(entry.title.clone())
                            .child(
                                div()
                                    .w_full()
                                    .text_left()
                                    .text_ellipsis()
                                    .overflow_hidden()
                                    .child(entry.title.clone()),
                            )
                            .on_click(cx.listener(move |this, _, _, cx| this.select(index, cx)))
                    })
                    .collect()
            }),
        )
        .h_full();
        let body = match &self.text {
            Some(text) => div().child(text.clone()),
            None => div().child(self.status.clone()),
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(0x222427))
            .text_color(rgb(0xf1eee7))
            .font_family("Segoe UI")
            .child(
                TitleBar::new().h(px(32.)).text_sm().child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            img("app-icon.png")
                                .w(px(20.))
                                .h(px(20.))
                                .object_fit(ObjectFit::Contain),
                        )
                        .child("Open-source licenses"),
                ),
            )
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .w(px(300.))
                            .flex_shrink_0()
                            .flex()
                            .flex_col()
                            .border_r_1()
                            .border_color(rgb(0x45464a))
                            .child(
                                div()
                                    .p_3()
                                    .text_sm()
                                    .child(format!("Packaged components ({})", self.entries.len())),
                            )
                            .child(div().flex_1().min_h_0().child(list)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .p_4()
                            .gap_3()
                            .child(
                                div().flex().justify_end().child(
                                    Button::new("copy-license-text")
                                        .ghost()
                                        .label("Copy full text")
                                        .disabled(self.text.is_none())
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            if let Some(text) = &this.text {
                                                cx.write_to_clipboard(ClipboardItem::new_string(
                                                    text.to_string(),
                                                ));
                                            }
                                        })),
                                ),
                            )
                            .child(
                                div()
                                    .id(("license-text", self.generation))
                                    .flex_1()
                                    .min_h_0()
                                    .overflow_y_scroll()
                                    .text_base()
                                    .line_height(px(24.))
                                    .child(body),
                            ),
                    ),
            )
    }
}
