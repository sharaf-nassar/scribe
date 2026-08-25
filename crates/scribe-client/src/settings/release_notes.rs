//! Release-note view model for the GPUI settings window.

use quick_xml::Reader;
use quick_xml::events::{BytesStart, Event};
use scribe_common::protocol::{Release, ReleaseListResultState};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReleaseNoteBlockKind {
    Heading,
    Paragraph,
    ListItem,
    Code,
    Quote,
    Link,
    Rule,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ReleaseNoteBlock {
    pub kind: ReleaseNoteBlockKind,
    pub text: String,
    pub target: Option<String>,
}

#[derive(Clone, Debug)]
pub(super) struct ReleasePanelItem {
    pub release: Release,
    pub blocks: Vec<ReleaseNoteBlock>,
}

impl ReleasePanelItem {
    pub fn body_link_targets(&self) -> impl Iterator<Item = &str> {
        self.blocks
            .iter()
            .filter_map(|block| block.target.as_deref())
            .filter(|target| is_http_url(target))
    }
}

impl From<Release> for ReleasePanelItem {
    fn from(release: Release) -> Self {
        let blocks = release_note_blocks(&release.body_html);
        Self { release, blocks }
    }
}

#[derive(Default)]
pub(super) enum ReleasePanelState {
    #[default]
    Unloaded,
    Loading,
    Ready {
        releases: Vec<ReleasePanelItem>,
        stale_reason: Option<String>,
    },
    Failed(String),
}

impl ReleasePanelState {
    pub fn selected(&self, index: usize) -> Option<&ReleasePanelItem> {
        match self {
            Self::Ready { releases, .. } => releases.get(index),
            Self::Unloaded | Self::Loading | Self::Failed(_) => None,
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Self::Ready { releases, .. } => releases.len(),
            Self::Unloaded | Self::Loading | Self::Failed(_) => 0,
        }
    }

    pub fn from_wire(state: ReleaseListResultState) -> Self {
        match state {
            ReleaseListResultState::Fresh { releases } => Self::Ready {
                releases: releases.into_iter().map(ReleasePanelItem::from).collect(),
                stale_reason: None,
            },
            ReleaseListResultState::Stale { releases, reason } => Self::Ready {
                releases: releases.into_iter().map(ReleasePanelItem::from).collect(),
                stale_reason: Some(reason),
            },
            ReleaseListResultState::Failed { reason } => Self::Failed(reason),
        }
    }
}

pub(super) fn adjacent_release_index(index: usize, len: usize, direction: isize) -> Option<usize> {
    if direction.is_negative() {
        index.checked_sub(1)
    } else {
        let next = index.checked_add(1)?;
        (next < len).then_some(next)
    }
}

pub(super) fn release_title(release: &Release) -> String {
    let version = release.version.trim();
    let named = release.name.as_deref().map(str::trim).filter(|name| !name.is_empty());
    match named {
        Some(name)
            if !name.eq_ignore_ascii_case(version)
                && !name.eq_ignore_ascii_case(&format!("v{version}")) =>
        {
            name.to_owned()
        }
        _ if version.is_empty() => "Scribe release".to_owned(),
        _ => format!("Scribe {version}"),
    }
}

pub(super) fn release_date(release: &Release) -> &str {
    release.published_at.split('T').next().unwrap_or(&release.published_at)
}

fn release_note_blocks(html: &str) -> Vec<ReleaseNoteBlock> {
    let mut reader = Reader::from_str(html);
    reader.config_mut().check_end_names = false;
    reader.config_mut().trim_text(false);
    let mut parser = ReleaseBlockParser::default();
    loop {
        match reader.read_event() {
            Ok(Event::Start(tag)) => parser.start(&tag),
            Ok(Event::Empty(tag)) => parser.empty(tag.name().as_ref()),
            Ok(Event::End(tag)) => parser.end(tag.name().as_ref()),
            Ok(Event::Text(text)) => parser.text(text.decode().ok().as_deref()),
            Ok(Event::GeneralRef(reference)) => {
                parser.reference(reference.decode().ok().as_deref());
            }
            Ok(Event::Eof) | Err(_) => break,
            Ok(_) => {}
        }
    }
    parser.finish()
}

#[derive(Default)]
struct ReleaseBlockParser {
    blocks: Vec<ReleaseNoteBlock>,
    current_kind: Option<ReleaseNoteBlockKind>,
    current: String,
    current_target: Option<String>,
    inline_code: bool,
}

impl ReleaseBlockParser {
    fn start(&mut self, tag: &BytesStart<'_>) {
        let name = tag.name();
        let tag_name = name.as_ref();
        if tag_name == b"a" {
            flush_block(
                &mut self.blocks,
                &mut self.current_kind,
                &mut self.current,
                &mut self.current_target,
            );
            self.current_kind = Some(ReleaseNoteBlockKind::Link);
            self.current_target = http_href(tag);
            return;
        }
        let kind = match tag_name {
            b"h1" | b"h2" | b"h3" | b"h4" | b"h5" | b"h6" => Some(ReleaseNoteBlockKind::Heading),
            b"p" if self.current_kind.is_none() => Some(ReleaseNoteBlockKind::Paragraph),
            b"li" => Some(ReleaseNoteBlockKind::ListItem),
            b"pre" => Some(ReleaseNoteBlockKind::Code),
            b"blockquote" if self.current_kind.is_none() => Some(ReleaseNoteBlockKind::Quote),
            b"tr" => Some(ReleaseNoteBlockKind::Paragraph),
            _ => None,
        };
        if let Some(kind) = kind {
            begin_block(
                &mut self.blocks,
                &mut self.current_kind,
                &mut self.current,
                &mut self.current_target,
                kind,
            );
        } else if tag_name == b"code" && self.current_kind != Some(ReleaseNoteBlockKind::Code) {
            self.inline_code = true;
            self.current.push('`');
        } else if tag_name == b"br" {
            self.current.push('\n');
        }
    }

    fn empty(&mut self, tag: &[u8]) {
        match tag {
            b"br" => self.current.push('\n'),
            b"hr" => {
                flush_block(
                    &mut self.blocks,
                    &mut self.current_kind,
                    &mut self.current,
                    &mut self.current_target,
                );
                self.blocks.push(ReleaseNoteBlock {
                    kind: ReleaseNoteBlockKind::Rule,
                    text: String::new(),
                    target: None,
                });
            }
            b"input" => push_text(&mut self.current, "☐", self.current_kind),
            _ => {}
        }
    }

    fn end(&mut self, tag: &[u8]) {
        if is_block_end(tag) || tag == b"a" {
            flush_block(
                &mut self.blocks,
                &mut self.current_kind,
                &mut self.current,
                &mut self.current_target,
            );
        } else if tag == b"code" && self.inline_code {
            self.current.push('`');
            self.inline_code = false;
        } else if matches!(tag, b"th" | b"td") && !self.current.trim().is_empty() {
            self.current.push_str("  ·  ");
        }
    }

    fn text(&mut self, decoded: Option<&str>) {
        let Some(decoded) = decoded else { return };
        if self.current_kind.is_none() && !decoded.trim().is_empty() {
            self.current_kind = Some(ReleaseNoteBlockKind::Paragraph);
        }
        match quick_xml::escape::unescape(decoded) {
            Ok(unescaped) => push_text(&mut self.current, &unescaped, self.current_kind),
            Err(_) => push_text(&mut self.current, decoded, self.current_kind),
        }
    }

    fn reference(&mut self, reference: Option<&str>) {
        let Some(reference) = reference else { return };
        let escaped = format!("&{reference};");
        let Ok(unescaped) = quick_xml::escape::unescape(&escaped) else { return };
        push_text(&mut self.current, &unescaped, self.current_kind);
    }

    fn finish(mut self) -> Vec<ReleaseNoteBlock> {
        flush_block(
            &mut self.blocks,
            &mut self.current_kind,
            &mut self.current,
            &mut self.current_target,
        );
        if self.blocks.is_empty() {
            self.blocks.push(ReleaseNoteBlock {
                kind: ReleaseNoteBlockKind::Paragraph,
                text: "No release notes were published for this release.".to_owned(),
                target: None,
            });
        }
        self.blocks
    }
}

fn is_block_end(tag: &[u8]) -> bool {
    matches!(
        tag,
        b"h1"
            | b"h2"
            | b"h3"
            | b"h4"
            | b"h5"
            | b"h6"
            | b"p"
            | b"li"
            | b"pre"
            | b"blockquote"
            | b"tr"
    )
}

fn begin_block(
    blocks: &mut Vec<ReleaseNoteBlock>,
    current_kind: &mut Option<ReleaseNoteBlockKind>,
    current: &mut String,
    current_target: &mut Option<String>,
    kind: ReleaseNoteBlockKind,
) {
    flush_block(blocks, current_kind, current, current_target);
    *current_kind = Some(kind);
}

fn flush_block(
    blocks: &mut Vec<ReleaseNoteBlock>,
    current_kind: &mut Option<ReleaseNoteBlockKind>,
    current: &mut String,
    current_target: &mut Option<String>,
) {
    let Some(kind) = current_kind.take() else {
        current.clear();
        return;
    };
    let text = current.trim().trim_end_matches('·').trim_end().to_owned();
    current.clear();
    if text.is_empty() {
        *current_target = None;
    } else {
        blocks.push(ReleaseNoteBlock { kind, text, target: current_target.take() });
    }
}

fn http_href(tag: &BytesStart<'_>) -> Option<String> {
    tag.attributes()
        .flatten()
        .find(|attribute| attribute.key.as_ref() == b"href")
        .and_then(|attribute| attribute.decode_and_unescape_value(tag.decoder()).ok())
        .map(std::borrow::Cow::into_owned)
        .filter(|target| is_http_url(target))
}

fn is_http_url(target: &str) -> bool {
    target.starts_with("https://") || target.starts_with("http://")
}

fn push_text(current: &mut String, text: &str, kind: Option<ReleaseNoteBlockKind>) {
    if kind == Some(ReleaseNoteBlockKind::Code) {
        current.push_str(text);
        return;
    }
    for word in text.split_whitespace() {
        if !current.is_empty() && !current.ends_with([' ', '\n', '`']) {
            current.push(' ');
        }
        current.push_str(word);
    }
}

#[cfg(test)]
mod tests {
    use super::{ReleaseNoteBlockKind, adjacent_release_index, release_note_blocks, release_title};
    use scribe_common::protocol::Release;

    fn release(name: Option<&str>) -> Release {
        Release {
            version: "1.2.3".to_owned(),
            name: name.map(str::to_owned),
            published_at: "2026-08-25T10:00:00Z".to_owned(),
            body_html: String::new(),
            prerelease: false,
            html_url: String::new(),
        }
    }

    #[test]
    fn title_uses_the_release_name_without_repeating_the_version() {
        assert_eq!(release_title(&release(Some("Fast panes"))), "Fast panes");
        assert_eq!(release_title(&release(Some("v1.2.3"))), "Scribe 1.2.3");
        assert_eq!(release_title(&release(Some("1.2.3"))), "Scribe 1.2.3");
    }

    #[test]
    fn navigation_stops_at_both_ends() {
        assert_eq!(adjacent_release_index(0, 3, -1), None);
        assert_eq!(adjacent_release_index(0, 3, 1), Some(1));
        assert_eq!(adjacent_release_index(2, 3, 1), None);
        assert_eq!(adjacent_release_index(2, 3, -1), Some(1));
    }

    #[test]
    fn html_becomes_readable_typed_blocks() {
        let blocks = release_note_blocks(
            "<h2>Highlights &amp; fixes</h2><p>Faster panes.</p><ul><li>One</li><li>Two</li></ul><pre><code>cargo test</code></pre>",
        );

        assert_eq!(blocks[0].kind, ReleaseNoteBlockKind::Heading);
        assert_eq!(blocks[0].text, "Highlights & fixes");
        assert_eq!(blocks[1].text, "Faster panes.");
        assert_eq!(blocks[2].kind, ReleaseNoteBlockKind::ListItem);
        assert_eq!(blocks[3].text, "Two");
        assert_eq!(blocks[4].kind, ReleaseNoteBlockKind::Code);
        assert_eq!(blocks[4].text, "cargo test");
    }

    #[test]
    fn http_links_keep_their_sanitized_target() {
        let blocks = release_note_blocks(
            "<p>Read <a href=\"https://example.com/notes\">full notes</a> now.</p>",
        );
        let link = blocks
            .iter()
            .find(|block| block.kind == ReleaseNoteBlockKind::Link)
            .expect("link block");

        assert_eq!(link.text, "full notes");
        assert_eq!(link.target.as_deref(), Some("https://example.com/notes"));
    }

    #[test]
    fn empty_body_has_a_clear_message() {
        assert_eq!(
            release_note_blocks("")[0].text,
            "No release notes were published for this release."
        );
    }
}
