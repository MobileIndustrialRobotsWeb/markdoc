use crate::ast::Node;
use crate::frontmatter;
use crate::tag_parser::{self, AttrValue, TagKind};
use crate::tokenizer::{Token, TokenEvent, TokenType, Tokenizer};
use crate::types::*;
use std::collections::HashMap;

/// Private-use sentinel standing in for an escaped `\{%` while the tag
/// scanners run, so they don't treat it as a tag opener. Restored to a
/// literal `{%` in node content by [`unescape_tag_literals`] afterwards.
const ESCAPED_TAG_OPEN: &str = "\u{E010}";

pub fn parse(content: &str, args: Option<ParserArgs>) -> Result<Node> {
    // Extract frontmatter.
    let (frontmatter_data, content_without_fm) = frontmatter::extract_frontmatter(content)?;

    // Neutralise escaped tag openers (`\{%`) before any tag scanner runs, so
    // they are never treated as tags. The sentinel is restored to a literal
    // `{%` in node content after parsing (`unescape_tag_literals`).
    let content_without_fm = content_without_fm.replace("\\{%", ESCAPED_TAG_OPEN);

    // Inline `{% $var %}` / `{% func(...) %}` interpolation is parsed into
    // interpolation nodes (by `segment_with_tags`) and resolved against the
    // evaluation `Context` at transform time — the same path as tag-attribute
    // expressions — so no text-level substitution happens here.

    // 1. Pre-extract Markdoc list-syntax `{% table %}` blocks. Their inner
    //    `---`/`*` syntax must be parsed here, before pulldown-cmark mangles
    //    it; each block becomes a placeholder spliced back in after parsing.
    let (content_with_tables, list_tables) =
        crate::list_table::extract_list_tables(&content_without_fm);

    // 2. Extract structural tags (`{% name attrs %}`, `{% /name %}`, etc.)
    //    into a side-table; replace each occurrence with a sentinel so the
    //    markdown tokenizer leaves them as opaque text. The tokenizer
    //    re-merges tag events back into the token stream.
    let (content_with_sentinels, parsed_tags) = tag_parser::segment_with_tags(&content_with_tables);

    let tokenizer = Tokenizer::new();
    let tokens = tokenizer.tokenize_with_tags(&content_with_sentinels, &parsed_tags);
    let tokens = lift_block_tags(tokens);
    let mut doc = parse_tokens(tokens, args)?;

    // Swap the list-table placeholders for their parsed `Table` nodes.
    crate::list_table::splice_list_tables(&mut doc, &list_tables);

    // Restore escaped `\{%` (now the sentinel) to a literal `{%` in text and
    // code content, after every tag scanner has run.
    unescape_tag_literals(&mut doc);

    if let Some(fm) = frontmatter_data {
        doc.attributes
            .insert("frontmatter".to_string(), Scalar::Object(fm));
    }

    Ok(doc)
}

/// Restore escaped tag openers ([`ESCAPED_TAG_OPEN`]) to a literal `{%` in
/// every node's text `content`, after the tag scanners have run.
fn unescape_tag_literals(node: &mut Node) {
    if let Some(Scalar::String(s)) = node.attributes.get_mut("content")
        && s.contains(ESCAPED_TAG_OPEN)
    {
        *s = s.replace(ESCAPED_TAG_OPEN, "{%");
    }
    for child in &mut node.children {
        unescape_tag_literals(child);
    }
}

/// Lift block-level Markdoc tags out of the paragraphs pulldown-cmark
/// wraps them in.
///
/// Tags are replaced with inline sentinels before markdown tokenization,
/// so pulldown sees a lone-on-its-line `{% tag %}` as inline text and
/// wraps it in a paragraph. A blank line inside a block tag then splits
/// the content into two paragraphs — the open sentinel trapped in the
/// first, the close sentinel in the second — and the first paragraph's
/// `End` event pops the still-open tag off the parse stack, truncating
/// everything after the blank line (it escapes as a sibling).
///
/// This pass walks each paragraph and splits it at every block-level tag
/// (a Markdoc `Open` / `Close` / `SelfClose` that sits alone on its
/// line — i.e. is bounded by a soft break or the paragraph edge on both
/// sides). Each such tag is emitted at block level; the inline runs
/// between them are re-wrapped as their own paragraphs. The result is
/// that `{% callout %}` spanning blank lines becomes a real container,
/// and `{% else /%}` stays a direct child of `{% if %}`. Tags that share
/// a line with other text (`see {% tagref /%} here`) are left inline.
fn lift_block_tags(tokens: Vec<Token>) -> Vec<Token> {
    let mut out: Vec<Token> = Vec::with_capacity(tokens.len());
    let mut i = 0;
    while i < tokens.len() {
        if matches!(tokens[i].event, TokenEvent::Start(TokenType::Paragraph)) {
            // Find the matching End(Paragraph). Paragraphs are leaf
            // blocks in CommonMark, so the next End(Paragraph) is ours.
            let para_start = &tokens[i];
            let end = (i + 1..tokens.len())
                .find(|&j| matches!(tokens[j].event, TokenEvent::End(TokenType::Paragraph)));
            let Some(end) = end else {
                // Unbalanced (shouldn't happen) — copy verbatim.
                out.push(tokens[i].clone());
                i += 1;
                continue;
            };
            let inner = &tokens[i + 1..end];
            let para_end = &tokens[end];
            split_paragraph_inner(inner, para_start, para_end, &mut out);
            i = end + 1;
        } else {
            out.push(tokens[i].clone());
            i += 1;
        }
    }
    out
}

/// Split one paragraph's `inner` token run at each block-level tag,
/// appending the result to `out`. `para_start` / `para_end` supply the
/// `Start`/`End(Paragraph)` tokens reused to re-wrap each inline segment.
fn split_paragraph_inner(
    inner: &[Token],
    para_start: &Token,
    para_end: &Token,
    out: &mut Vec<Token>,
) {
    let n = inner.len();
    // Mark which indices are block-level tags (alone on their line).
    let is_block: Vec<bool> = (0..n).map(|k| is_block_tag_at(inner, k)).collect();
    if !is_block.iter().any(|b| *b) {
        // No block tags — emit the paragraph untouched.
        out.push(para_start.clone());
        out.extend_from_slice(inner);
        out.push(para_end.clone());
        return;
    }

    let mut seg: Vec<Token> = Vec::new();
    let mut k = 0;
    while k < n {
        if is_block[k] {
            // Drop a soft break that trails the current segment (the one
            // that separated it from this tag) before flushing.
            trim_trailing_softbreak(&mut seg);
            flush_segment(&mut seg, para_start, para_end, out);
            out.push(inner[k].clone()); // the tag, now block-level
            // Skip a soft break immediately following the tag.
            if k + 2 < n && is_sb_start(&inner[k + 1].event) && is_sb_end(&inner[k + 2].event) {
                k += 3;
            } else {
                k += 1;
            }
        } else {
            seg.push(inner[k].clone());
            k += 1;
        }
    }
    trim_trailing_softbreak(&mut seg);
    flush_segment(&mut seg, para_start, para_end, out);
}

/// True when `inner[k]` is a Markdoc block container tag (`Open` /
/// `Close` / `SelfClose`) that is alone on its line — bounded on each
/// side by a soft break or the paragraph edge. Heading-id sugar and
/// inline tags (sharing a line with text) return false.
fn is_block_tag_at(inner: &[Token], k: usize) -> bool {
    let is_tag = matches!(
        inner[k].event,
        TokenEvent::Tag(TagKind::Open { .. } | TagKind::Close { .. } | TagKind::SelfClose { .. })
    );
    if !is_tag {
        return false;
    }
    let left_ok =
        k == 0 || (k >= 2 && is_sb_end(&inner[k - 1].event) && is_sb_start(&inner[k - 2].event));
    let right_ok = k + 1 == inner.len()
        || (k + 2 < inner.len()
            && is_sb_start(&inner[k + 1].event)
            && is_sb_end(&inner[k + 2].event));
    left_ok && right_ok
}

fn is_sb_start(ev: &TokenEvent) -> bool {
    matches!(ev, TokenEvent::Start(TokenType::SoftBreak))
}

fn is_sb_end(ev: &TokenEvent) -> bool {
    matches!(ev, TokenEvent::End(TokenType::SoftBreak))
}

/// Remove a trailing soft break (Start+End pair) from a segment.
fn trim_trailing_softbreak(seg: &mut Vec<Token>) {
    let n = seg.len();
    if n >= 2 && is_sb_start(&seg[n - 2].event) && is_sb_end(&seg[n - 1].event) {
        seg.truncate(n - 2);
    }
}

/// Wrap a non-empty inline segment as a paragraph and append it to
/// `out`, draining `seg`. A leading soft break is trimmed first; an
/// all-whitespace / empty segment emits nothing.
fn flush_segment(seg: &mut Vec<Token>, para_start: &Token, para_end: &Token, out: &mut Vec<Token>) {
    // Trim a leading soft break.
    if seg.len() >= 2 && is_sb_start(&seg[0].event) && is_sb_end(&seg[1].event) {
        seg.drain(0..2);
    }
    if seg.is_empty() {
        return;
    }
    out.push(para_start.clone());
    out.append(seg);
    out.push(para_end.clone());
}

fn parse_tokens(tokens: Vec<Token>, args: Option<ParserArgs>) -> Result<Node> {
    let args = args.unwrap_or(ParserArgs {
        file: None,
        slots: false,
        location: true,
    });

    let mut doc = Node::new(NodeType::Document, HashMap::new(), Vec::new(), None);
    let mut stack: Vec<Node> = vec![doc.clone()];
    let line_num = 1;

    for token in tokens {
        match token.event {
            TokenEvent::Start(token_type) => {
                let node = create_node_from_token(&token_type, line_num, &args);
                stack.push(node);
            }
            TokenEvent::End(_token_type) => {
                if stack.len() > 1 {
                    // `close_to_tag` may have already attached open markdown
                    // containers (lists, paragraphs, …) to a Markdoc tag that
                    // closed mid-structure. Pulldown-cmark still emits the
                    // matching End events afterward; ignoring them when a
                    // Markdoc tag is on top keeps `{% if %}` / `{% callout %}`
                    // from being popped early (which would leak later siblings
                    // out of the conditional — e.g. a list inside a callout
                    // inside an if, then a blank line, then another callout).
                    if matches!(stack.last().map(|n| &n.node_type), Some(NodeType::Tag)) {
                        continue;
                    }
                    let node = stack.pop().unwrap();

                    if let Some(parent) = stack.last_mut() {
                        if args.slots && is_slot_node(&node) {
                            if let Some(Scalar::String(slot_name)) = node.attributes.get("primary")
                            {
                                parent.slots.insert(slot_name.clone(), node);
                            } else {
                                parent.push(node);
                            }
                        } else {
                            parent.push(node);
                        }
                    }
                }
            }
            TokenEvent::Text(text) => {
                if let Some(parent) = stack.last_mut() {
                    let mut attrs = HashMap::new();
                    attrs.insert("content".to_string(), Scalar::String(text));
                    let text_node = Node::new(NodeType::Text, attrs, Vec::new(), None);
                    parent.push(text_node);
                }
            }
            TokenEvent::Code(code) => {
                if let Some(parent) = stack.last_mut() {
                    let mut attrs = HashMap::new();
                    attrs.insert("content".to_string(), Scalar::String(code));
                    let code_node = Node::new(NodeType::Code, attrs, Vec::new(), None);
                    parent.push(code_node);
                }
            }
            TokenEvent::Html(html) => {
                // Markdoc has no raw-HTML support. Rather than silently
                // dropping the markup — which loses literal placeholder
                // text such as `<insert …>` — surface it as text so it
                // renders verbatim. HTML comments stay hidden.
                if !html.trim_start().starts_with("<!--")
                    && let Some(parent) = stack.last_mut()
                {
                    let mut attrs = HashMap::new();
                    attrs.insert("content".to_string(), Scalar::String(html));
                    let text_node = Node::new(NodeType::Text, attrs, Vec::new(), None);
                    parent.push(text_node);
                }
            }
            TokenEvent::Tag(kind) => {
                handle_tag_event(kind, &mut stack, line_num, &args);
            }
        }

        // Track line numbers (simplified)
        if let Some((_, _end_pos)) = token.position {
            // TODO This is a simplification - in a real implementation,
            // we'd need to track actual line numbers from the source
        }
    }

    // Pop remaining nodes and add to parent
    while stack.len() > 1 {
        let node = stack.pop().unwrap();
        if let Some(parent) = stack.last_mut() {
            parent.push(node);
        }
    }

    doc = stack.pop().unwrap();
    Ok(doc)
}

fn create_node_from_token(token_type: &TokenType, line: usize, args: &ParserArgs) -> Node {
    let (node_type, attributes) = match token_type {
        TokenType::Paragraph => (NodeType::Paragraph, HashMap::new()),
        TokenType::Heading(level) => {
            let mut attrs = HashMap::new();
            attrs.insert("level".to_string(), Scalar::Number(*level as f64));
            (NodeType::Heading, attrs)
        }
        TokenType::BlockQuote => (NodeType::Blockquote, HashMap::new()),
        TokenType::CodeBlock(lang) => {
            let mut attrs = HashMap::new();
            if let Some(language) = lang {
                attrs.insert("language".to_string(), Scalar::String(language.clone()));
            }
            (NodeType::Fence, attrs)
        }
        TokenType::List(ordered, start) => {
            let mut attrs = HashMap::new();
            attrs.insert("ordered".to_string(), Scalar::Boolean(*ordered));
            if let Some(start_num) = start {
                attrs.insert("start".to_string(), Scalar::Number(*start_num as f64));
            }
            (NodeType::List, attrs)
        }
        TokenType::Item => (NodeType::Item, HashMap::new()),
        TokenType::Table(aligns) => {
            let mut attrs = HashMap::new();
            // Carry CommonMark per-column alignment (from the `:--`/`--:`
            // delimiter row) so the renderer can align cells. Omitted when
            // no column declares an explicit alignment.
            if aligns.iter().any(|a| !a.is_empty()) {
                attrs.insert(
                    "align".to_string(),
                    Scalar::Array(aligns.iter().cloned().map(Scalar::String).collect()),
                );
            }
            (NodeType::Table, attrs)
        }
        TokenType::TableHead => (NodeType::Thead, HashMap::new()),
        TokenType::TableRow => (NodeType::Tr, HashMap::new()),
        TokenType::TableCell => (NodeType::Td, HashMap::new()),
        TokenType::Emphasis => (NodeType::Em, HashMap::new()),
        TokenType::Strong => (NodeType::Strong, HashMap::new()),
        TokenType::Strikethrough => (NodeType::Strikethrough, HashMap::new()),
        TokenType::Link(url, title) => {
            let mut attrs = HashMap::new();
            attrs.insert("href".to_string(), Scalar::String(url.clone()));
            if !title.is_empty() {
                attrs.insert("title".to_string(), Scalar::String(title.clone()));
            }
            (NodeType::Link, attrs)
        }
        TokenType::Image(url, title) => {
            let mut attrs = HashMap::new();
            attrs.insert("src".to_string(), Scalar::String(url.clone()));
            if !title.is_empty() {
                attrs.insert("title".to_string(), Scalar::String(title.clone()));
            }
            (NodeType::Image, attrs)
        }
        TokenType::Rule => (NodeType::Hr, HashMap::new()),
        TokenType::LineBreak => (NodeType::Hardbreak, HashMap::new()),
        TokenType::SoftBreak => (NodeType::Softbreak, HashMap::new()),
        // CommonMark footnotes — preserved as tag nodes carrying the label;
        // `resolve_footnotes` matches references to definitions later.
        TokenType::FootnoteRef(name) | TokenType::FootnoteDef(name) => {
            let mut attrs = HashMap::new();
            attrs.insert("name".to_string(), Scalar::String(name.clone()));
            (NodeType::Tag, attrs)
        }
    };

    let mut node = Node::new(node_type, attributes, Vec::new(), None);
    match token_type {
        TokenType::FootnoteRef(_) => node.tag = Some("footnote-ref".to_string()),
        TokenType::FootnoteDef(_) => node.tag = Some("footnote-def".to_string()),
        _ => {}
    }

    if args.location {
        node.lines = vec![line];
        node.location = Some(Location {
            file: args.file.clone(),
            start: LocationEdge {
                line,
                character: None,
            },
            end: LocationEdge {
                line,
                character: None,
            },
        });
    }

    node
}

fn is_slot_node(node: &Node) -> bool {
    node.tag.as_deref() == Some("slot")
}

/// Convert a parsed tag event into stack manipulation:
///   - Open       → push a new `Tag` node
///   - Close      → pop nodes until the matching `Tag` node is closed and
///     attached to its parent (closing markdown nodes that were
///     still open are attached too, in source order)
///   - SelfClose  → push then immediately pop
///   - HeadingId  → store the id on the nearest enclosing heading
fn handle_tag_event(kind: TagKind, stack: &mut Vec<Node>, line: usize, args: &ParserArgs) {
    match kind {
        TagKind::Open { name, attrs } => {
            stack.push(make_tag_node(&name, attrs, line, args));
        }
        TagKind::SelfClose { name, attrs } => {
            stack.push(make_tag_node(&name, attrs, line, args));
            close_to_tag(&name, stack);
        }
        TagKind::Close { name } => {
            close_to_tag(&name, stack);
        }
        TagKind::HeadingId { id } => {
            // Walk the stack backwards looking for the nearest open heading.
            for node in stack.iter_mut().rev() {
                if matches!(node.node_type, NodeType::Heading) {
                    node.attributes.insert("id".to_string(), Scalar::String(id));
                    return;
                }
            }
            // No enclosing heading — silently drop. (The Markdoc spec
            // restricts this sugar to heading contexts.)
        }
        TagKind::Interpolation { expr } => {
            // An inline leaf carrying the expression under `primary`; the
            // transformer evaluates it against the Context into text.
            let mut node = Node::new(
                NodeType::Tag,
                HashMap::new(),
                Vec::new(),
                Some(INTERPOLATION_TAG.to_string()),
            );
            node.expressions.insert("primary".to_string(), expr);
            node.inline = true;
            if let Some(parent) = stack.last_mut() {
                parent.push(node);
            }
        }
    }
}

/// Internal tag name for an inline interpolation node (`{% $var %}` /
/// `{% func(...) %}`). Resolved to text by the transformer; never authored.
pub(crate) const INTERPOLATION_TAG: &str = "$interpolation";

/// Build a `Tag` node from parsed attributes, splitting them into the
/// `attributes` map (literal scalars) and the `expressions` map (raw
/// expression source for variables and function calls).
fn make_tag_node(name: &str, attrs: tag_parser::TagAttrs, line: usize, args: &ParserArgs) -> Node {
    let mut attributes: HashMap<String, Scalar> = HashMap::new();
    let mut expressions: HashMap<String, String> = HashMap::new();

    if let Some(primary) = attrs.primary {
        match primary {
            AttrValue::Literal(s) => {
                attributes.insert("primary".to_string(), s);
            }
            AttrValue::Expression(src) => {
                expressions.insert("primary".to_string(), src);
            }
        }
    }
    for (key, value) in attrs.named {
        match value {
            AttrValue::Literal(s) => {
                attributes.insert(key, s);
            }
            AttrValue::Expression(src) => {
                expressions.insert(key, src);
            }
        }
    }

    let mut node = Node::new(
        NodeType::Tag,
        attributes,
        Vec::new(),
        Some(name.to_string()),
    );
    node.expressions = expressions;
    if args.location {
        node.lines = vec![line];
        node.location = Some(Location {
            file: args.file.clone(),
            start: LocationEdge {
                line,
                character: None,
            },
            end: LocationEdge {
                line,
                character: None,
            },
        });
    }
    node
}

/// Pop nodes off the stack and attach them to their parents until a `Tag`
/// node with the given name is itself popped and attached. If no such tag
/// is open, the call is a no-op (mismatched close — silently ignored for now;
/// future work: emit a ValidationError).
fn close_to_tag(name: &str, stack: &mut Vec<Node>) {
    // Find the position of the matching tag on the stack.
    let target = stack
        .iter()
        .rposition(|n| matches!(n.node_type, NodeType::Tag) && n.tag.as_deref() == Some(name));
    let Some(target) = target else { return };

    // Pop everything above the target into their parents.
    while stack.len() > target + 1 {
        let node = stack.pop().unwrap();
        if let Some(parent) = stack.last_mut() {
            parent.push(node);
        }
    }
    // Pop the target itself and attach to its parent.
    let tag_node = stack.pop().unwrap();
    if let Some(parent) = stack.last_mut() {
        parent.push(tag_node);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Walk the tree and find the first node satisfying `pred`.
    fn find<'a>(root: &'a Node, pred: &dyn Fn(&Node) -> bool) -> Option<&'a Node> {
        if pred(root) {
            return Some(root);
        }
        for child in &root.children {
            if let Some(found) = find(child, pred) {
                return Some(found);
            }
        }
        None
    }

    #[test]
    fn parses_simple_markdown_unchanged() {
        let src = "# Hello\n\nThis is **bold**.";
        let doc = parse(src, None).unwrap();
        let heading =
            find(&doc, &|n| matches!(n.node_type, NodeType::Heading)).expect("heading present");
        assert_eq!(heading.attributes.get("level"), Some(&Scalar::Number(1.0)));
        let strong =
            find(&doc, &|n| matches!(n.node_type, NodeType::Strong)).expect("strong present");
        let text =
            find(strong, &|n| matches!(n.node_type, NodeType::Text)).expect("text in strong");
        assert_eq!(
            text.attributes.get("content"),
            Some(&Scalar::String("bold".to_string()))
        );
    }

    #[test]
    fn escaped_tag_opener_stays_literal() {
        // `\{%` escapes a tag opener: it stays literal text (in prose and
        // inside inline code) and is never parsed as a tag, while a real
        // unescaped tag beside it still parses.
        let src = "a \\{% if $x %} b and `\\{% c %}`\n\n{% callout %}\nreal\n{% /callout %}";
        let doc = parse(src, None).unwrap();

        // Prose keeps the literal `{% if $x %}`.
        assert!(
            find(&doc, &|n| matches!(n.node_type, NodeType::Text)
                && matches!(n.attributes.get("content"),
                    Some(Scalar::String(s)) if s.contains("{% if $x %}")))
            .is_some(),
            "escaped opener should stay literal in prose: {doc:#?}"
        );

        // Inline code keeps the literal `{% c %}` as its content.
        let code = find(&doc, &|n| matches!(n.node_type, NodeType::Code)).expect("inline code");
        assert_eq!(
            code.attributes.get("content"),
            Some(&Scalar::String("{% c %}".to_string()))
        );

        // The escaped openers produced no tag; the real callout still did.
        assert!(
            find(&doc, &|n| n.tag.as_deref() == Some("if")
                || n.tag.as_deref() == Some("c"))
            .is_none(),
            "escaped openers must not become tags: {doc:#?}"
        );
        assert!(
            find(&doc, &|n| n.tag.as_deref() == Some("callout")).is_some(),
            "real callout tag must still parse: {doc:#?}"
        );
    }

    #[test]
    fn parses_callout_as_tag_node() {
        let src = r#"{% callout type="warning" %}
**Warn**: be careful.
{% /callout %}"#;
        let doc = parse(src, None).unwrap();

        let callout = find(&doc, &|n| {
            matches!(n.node_type, NodeType::Tag) && n.tag.as_deref() == Some("callout")
        })
        .expect("callout tag node present");

        assert_eq!(
            callout.attributes.get("type"),
            Some(&Scalar::String("warning".to_string()))
        );
        // The callout should contain the `**Warn**: be careful.` content.
        let strong = find(callout, &|n| matches!(n.node_type, NodeType::Strong))
            .expect("bold inside callout");
        let warn = find(strong, &|n| matches!(n.node_type, NodeType::Text)).expect("warn text");
        assert_eq!(
            warn.attributes.get("content"),
            Some(&Scalar::String("Warn".to_string()))
        );
    }

    #[test]
    fn parses_underscore_emphasis_with_interpolated_variable() {
        // Emphasis delimiters still wrap an inline `{% $var %}` interpolation
        // node (resolved later at transform time).
        let src = "* _{% $model %} Manual_\n";
        let doc = parse(src, None).unwrap();
        let em = find(&doc, &|n| matches!(n.node_type, NodeType::Em)).expect("em present");
        let interp = find(em, &|n| n.tag.as_deref() == Some(INTERPOLATION_TAG))
            .expect("interpolation inside em");
        assert_eq!(
            interp.expressions.get("primary").map(String::as_str),
            Some("$model")
        );
        let text = find(em, &|n| matches!(n.node_type, NodeType::Text)).expect("text in em");
        assert_eq!(
            text.attributes.get("content"),
            Some(&Scalar::String(" Manual".to_string()))
        );
    }

    #[test]
    fn parses_bold_with_interpolated_variable() {
        let src = "**{% $mir_fleet %}**\n";
        let doc = parse(src, None).unwrap();
        let strong =
            find(&doc, &|n| matches!(n.node_type, NodeType::Strong)).expect("strong present");
        let interp = find(strong, &|n| n.tag.as_deref() == Some(INTERPOLATION_TAG))
            .expect("interpolation inside strong");
        assert_eq!(
            interp.expressions.get("primary").map(String::as_str),
            Some("$mir_fleet")
        );
    }

    #[test]
    fn spaced_underscores_do_not_form_emphasis() {
        let src = "* _ {% $model %} Manual _\n";
        let doc = parse(src, None).unwrap();
        assert!(
            find(&doc, &|n| matches!(n.node_type, NodeType::Em)).is_none(),
            "spaces inside _ delimiters must prevent emphasis"
        );
    }

    #[test]
    fn indented_bold_inside_if_is_not_emphasis() {
        // Four-space indent makes this a code block in CommonMark, not bold.
        let src = r#"{% if $show %}

    **MiR Fleet**

{% /if %}"#;
        let doc = parse(src, None).unwrap();
        assert!(
            find(&doc, &|n| matches!(n.node_type, NodeType::Strong)).is_none(),
            "indented bold inside if-block is not parsed as Strong"
        );
    }

    #[test]
    fn dedented_bold_inside_if_parses_as_strong() {
        let src = r#"{% if $show %}

**MiR Fleet**

{% /if %}"#;
        let doc = parse(src, None).unwrap();
        let strong =
            find(&doc, &|n| matches!(n.node_type, NodeType::Strong)).expect("strong present");
        let text =
            find(strong, &|n| matches!(n.node_type, NodeType::Text)).expect("text in strong");
        assert_eq!(
            text.attributes.get("content"),
            Some(&Scalar::String("MiR Fleet".to_string()))
        );
    }

    #[test]
    fn captures_heading_id_sugar() {
        let src = "# Overview {% #my-overview %}";
        let doc = parse(src, None).unwrap();
        let heading =
            find(&doc, &|n| matches!(n.node_type, NodeType::Heading)).expect("heading present");
        assert_eq!(
            heading.attributes.get("id"),
            Some(&Scalar::String("my-overview".to_string()))
        );
    }

    #[test]
    fn captures_unresolved_expressions_in_node_expressions_field() {
        let src = "{% if $config.show_section %}content{% /if %}";
        let doc = parse(src, None).unwrap();
        let if_node = find(&doc, &|n| {
            matches!(n.node_type, NodeType::Tag) && n.tag.as_deref() == Some("if")
        })
        .expect("if tag node");
        // The primary expression should land in `expressions["primary"]`,
        // not in `attributes` (because it's an unresolved expression).
        assert!(!if_node.attributes.contains_key("primary"));
        assert_eq!(
            if_node.expressions.get("primary").map(String::as_str),
            Some("$config.show_section")
        );
    }

    #[test]
    fn self_closing_partial_becomes_empty_tag_node() {
        let src = r#"{% partial file="x.markdoc" /%}"#;
        let doc = parse(src, None).unwrap();
        let partial = find(&doc, &|n| {
            matches!(n.node_type, NodeType::Tag) && n.tag.as_deref() == Some("partial")
        })
        .expect("partial tag");
        assert_eq!(
            partial.attributes.get("file"),
            Some(&Scalar::String("x.markdoc".to_string()))
        );
        assert!(partial.children.is_empty());
    }

    /// Count direct children of `node` that satisfy `pred`.
    fn count_children(node: &Node, pred: &dyn Fn(&Node) -> bool) -> usize {
        node.children.iter().filter(|c| pred(c)).count()
    }

    fn is_para(n: &Node) -> bool {
        matches!(n.node_type, NodeType::Paragraph)
    }

    fn tag_named<'a>(root: &'a Node, name: &str) -> Option<&'a Node> {
        find(root, &|n| {
            matches!(n.node_type, NodeType::Tag) && n.tag.as_deref() == Some(name)
        })
    }

    #[test]
    fn block_tag_spanning_blank_line_keeps_all_content() {
        // The regression: a blank line inside a block tag used to close it
        // early, leaking the second paragraph out as a sibling.
        let src = "{% callout %}\nFirst para.\n\nSecond para.\n{% /callout %}\n";
        let doc = parse(src, None).unwrap();
        let callout = tag_named(&doc, "callout").expect("callout present");
        // Both paragraphs are now children of the callout.
        assert_eq!(
            count_children(callout, &is_para),
            2,
            "callout should own both paragraphs, tree was {doc:#?}"
        );
        // And nothing escaped to the document root besides the callout.
        assert_eq!(count_children(&doc, &is_para), 0);
    }

    #[test]
    fn block_tag_with_list_keeps_list_inside() {
        let src = "{% callout %}\nIntro.\n\n- a\n- b\n\n{% /callout %}\n";
        let doc = parse(src, None).unwrap();
        let callout = tag_named(&doc, "callout").expect("callout present");
        assert_eq!(count_children(callout, &is_para), 1);
        assert_eq!(
            count_children(callout, &|n| matches!(n.node_type, NodeType::List)),
            1,
            "list should live inside the callout"
        );
    }

    #[test]
    fn else_stays_direct_child_of_if() {
        // `{% else /%}` alone on its line must split the paragraph so it
        // remains a direct child of `{% if %}` (conditional evaluation
        // keys on that), even without blank lines around it.
        let src = "{% if $a %}\nA content\n{% else /%}\nB content\n{% /if %}\n";
        let doc = parse(src, None).unwrap();
        let if_node = tag_named(&doc, "if").expect("if present");
        assert_eq!(
            count_children(if_node, &|n| matches!(n.node_type, NodeType::Tag)
                && n.tag.as_deref() == Some("else")),
            1,
            "else must be a direct child of if, tree was {doc:#?}"
        );
        // Each branch's content is wrapped in its own paragraph.
        assert_eq!(count_children(if_node, &is_para), 2);
    }

    #[test]
    fn list_inside_callout_inside_if_keeps_following_siblings() {
        // Regression: a markdown list inside `{% callout %}` leaves
        // pending List/Item/Paragraph End events after `{% /callout %}`.
        // Those must not pop the enclosing `{% if %}`, or later callouts
        // (and `{% else / %}`) escape the conditional.
        let src = r#"{% if $x %}
{% callout type="warning" %}
Intro text.
* first item

* second item
{% /callout %}

{% callout type="warning" %}
Second callout
{% /callout %}
{% /if %}
"#;
        let doc = parse(src, None).unwrap();
        assert_eq!(
            count_children(&doc, &|n| matches!(n.node_type, NodeType::Tag)
                && n.tag.as_deref() == Some("if")),
            1,
            "exactly one top-level if, tree was {doc:#?}"
        );
        let if_node = tag_named(&doc, "if").unwrap();
        assert_eq!(
            count_children(if_node, &|n| matches!(n.node_type, NodeType::Tag)
                && n.tag.as_deref() == Some("callout")),
            2,
            "both callouts must stay inside if, tree was {doc:#?}"
        );
        assert_eq!(
            count_children(&doc, &|n| matches!(n.node_type, NodeType::Tag)
                && n.tag.as_deref() == Some("callout")),
            0,
            "no callout may leak to document root, tree was {doc:#?}"
        );
    }

    #[test]
    fn list_inside_callout_then_else_stays_inside_if() {
        let src = r#"{% if $top_module %}
{% callout type="warning" %}
If the robot transports a load that is larger than the supported size.

* Do not load the robot with loads that extend beyond supported dimensions.
{% /callout %}

{% else / %}

{% callout type="warning" %}
Else branch callout
{% /callout %}
{% /if %}
"#;
        let doc = parse(src, None).unwrap();
        let if_node = tag_named(&doc, "if").expect("if present");
        assert_eq!(
            count_children(if_node, &|n| matches!(n.node_type, NodeType::Tag)
                && n.tag.as_deref() == Some("else")),
            1,
            "else must be a direct child of if, tree was {doc:#?}"
        );
        assert_eq!(
            count_children(if_node, &|n| matches!(n.node_type, NodeType::Tag)
                && n.tag.as_deref() == Some("callout")),
            2,
            "both branch callouts must stay inside if, tree was {doc:#?}"
        );
        assert_eq!(
            count_children(&doc, &|n| matches!(n.node_type, NodeType::Tag)
                && n.tag.as_deref() == Some("else")),
            0,
            "else must not leak to document root, tree was {doc:#?}"
        );
    }

    #[test]
    fn inline_tag_sharing_a_line_stays_inline() {
        // A tag with text on the same line is inline and must NOT be
        // lifted to block level.
        let src = "See {% tagref id=\"x\" /%} for details.\n";
        let doc = parse(src, None).unwrap();
        // The tagref sits inside the single paragraph, not at doc root.
        assert_eq!(count_children(&doc, &is_para), 1);
        let para = doc.children.iter().find(|c| is_para(c)).unwrap();
        assert_eq!(
            count_children(para, &|n| matches!(n.node_type, NodeType::Tag)
                && n.tag.as_deref() == Some("tagref")),
            1
        );
    }

    #[test]
    fn standalone_self_closing_tag_lifts_to_block() {
        // A self-closing tag alone in its own paragraph becomes a
        // block-level node with the empty paragraph dropped.
        let src = "Before.\n\n{% partial file=\"x.markdoc\" /%}\n\nAfter.\n";
        let doc = parse(src, None).unwrap();
        let partial = tag_named(&doc, "partial").expect("partial present");
        // partial is a direct child of the document, between two paragraphs.
        assert_eq!(
            count_children(&doc, &|n| matches!(n.node_type, NodeType::Tag)
                && n.tag.as_deref() == Some("partial")),
            1
        );
        assert!(partial.children.is_empty());
        assert_eq!(count_children(&doc, &is_para), 2);
    }

    #[test]
    fn frontmatter_interpolation_parses_to_a_node() {
        // `{% $markdoc.frontmatter.title %}` is captured as an interpolation
        // node (resolved against the Context at transform time), not
        // substituted during parse.
        let src = "---\ntitle: Hello\n---\n\n# {% $markdoc.frontmatter.title %}";
        let doc = parse(src, None).unwrap();
        let node = find(&doc, &|n| n.tag.as_deref() == Some(INTERPOLATION_TAG))
            .expect("interpolation node present");
        assert_eq!(
            node.expressions.get("primary").map(String::as_str),
            Some("$markdoc.frontmatter.title")
        );
    }

    #[test]
    fn raw_html_renders_as_literal_text_but_comments_stay_hidden() {
        // Markdoc has no raw-HTML support, but an unrecognised tag-like
        // token (a placeholder such as `<insert …>`) must survive as
        // literal text instead of being silently dropped.
        let doc = parse("Product: <insert list of robots and HW versions>\n", None).unwrap();
        let kept = find(&doc, &|n| {
            matches!(n.node_type, NodeType::Text)
                && matches!(
                    n.attributes.get("content"),
                    Some(Scalar::String(s)) if s.contains("<insert list of robots and HW versions>")
                )
        });
        assert!(
            kept.is_some(),
            "raw `<insert …>` should survive as literal text, tree was {doc:#?}"
        );

        // HTML comments, by contrast, must stay hidden — their content
        // must never leak into the rendered text.
        let doc2 = parse("Before <!-- secret note --> after.\n", None).unwrap();
        let leaked = find(&doc2, &|n| {
            matches!(n.node_type, NodeType::Text)
                && matches!(
                    n.attributes.get("content"),
                    Some(Scalar::String(s)) if s.contains("secret")
                )
        });
        assert!(
            leaked.is_none(),
            "HTML comment content must not render, tree was {doc2:#?}"
        );
    }

    #[test]
    fn pipe_table_preserves_column_alignment() {
        // The `:--`/`:-:`/`--:` delimiter row must survive as an `align`
        // attribute on the table node so renderers can align columns.
        let src = "| L | C | R |\n|:--|:-:|--:|\n| a | b | c |\n";
        let doc = parse(src, None).unwrap();
        let table = find(&doc, &|n| matches!(n.node_type, NodeType::Table)).expect("table present");
        match table.attributes.get("align") {
            Some(Scalar::Array(a)) => {
                let names: Vec<&str> = a
                    .iter()
                    .filter_map(|s| match s {
                        Scalar::String(x) => Some(x.as_str()),
                        _ => None,
                    })
                    .collect();
                assert_eq!(names, vec!["left", "center", "right"]);
            }
            other => panic!("expected an align array, got {other:?}"),
        }

        // A table with no explicit alignment carries no `align` attribute.
        let plain = parse("| A | B |\n|---|---|\n| a | b |\n", None).unwrap();
        let t2 = find(&plain, &|n| matches!(n.node_type, NodeType::Table)).expect("table");
        assert!(!t2.attributes.contains_key("align"));
    }
}
