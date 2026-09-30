mod code_action_menu;
mod completion_menu;
mod diagnostic_popover;
mod hover_popover;

pub(crate) use code_action_menu::*;
pub(crate) use completion_menu::*;
pub(crate) use diagnostic_popover::*;
pub(crate) use hover_popover::*;

use gpui::{
    App, Div, ElementId, InteractiveElement as _, SharedString, Stateful, StyleRefinement,
    Styled as _, Window, div, px, rems,
};

use crate::{
    ActiveTheme, ThemeStyled as _,
    text::{TextView, TextViewStyle},
};

pub(super) fn render_markdown(
    id: impl Into<ElementId>,
    markdown: impl Into<SharedString>,
    _: &mut Window,
    cx: &mut App,
) -> TextView {
    TextView::markdown(id, markdown)
        .style(
            TextViewStyle::default()
                .paragraph_gap(rems(0.5))
                .heading_font_size(|level, rem_size| match level {
                    1..=3 => rem_size * 1,
                    4 => rem_size * 0.9,
                    _ => rem_size * 0.8,
                })
                .code_block(
                    StyleRefinement::default()
                        .bg(cx.theme().transparent)
                        .p_0()
                        .text_size(px(11.)),
                ),
        )
        .selectable(true)
}

/// Plain text as Markdown that draws it as it is: a problem's message, or a
/// hover in plain text, which VS Code and IntelliJ draw as plain text.
///
/// Its lines are its own, as rustc's `expected …, found …` under its first
/// line, where Markdown draws a line ending inside a paragraph as a space;
/// each is a hard break instead. Every ASCII punctuation character is
/// escaped, as CommonMark allows for any of them: jdtls's `List<String>`
/// had been an HTML tag and dropped, `a*b*c` emphasis, a leading `-` a list
/// and backticks code. A line's indentation is kept as no-break spaces,
/// where Markdown drops it after a line break and makes four spaces a
/// block of code.
pub(super) fn plain_text_as_markdown(text: &str) -> String {
    text.lines()
        .map(|line| {
            let line = line.trim_end();
            let body = line.trim_start_matches([' ', '\t']);
            let mut out = String::with_capacity(line.len() * 2);
            for c in line[..line.len() - body.len()].chars() {
                out.push_str(if c == '\t' {
                    "\u{a0}\u{a0}\u{a0}\u{a0}"
                } else {
                    "\u{a0}"
                });
            }
            for c in body.chars() {
                if c.is_ascii_punctuation() {
                    out.push('\\');
                }
                out.push(c);
            }
            out
        })
        .collect::<Vec<_>>()
        .join("  \n")
}

pub(super) fn editor_popover(id: impl Into<ElementId>, cx: &App) -> Stateful<Div> {
    div()
        .id(id)
        .flex_none()
        .occlude()
        .popover_style(cx)
        .shadow_md()
        .text_xs()
        .p_1()
}

#[cfg(test)]
mod tests {
    use super::{hover_markdown, plain_text_as_markdown};
    use crate::text::TextViewState;
    use gpui::{AppContext as _, TestAppContext};

    /// The text `markdown` draws in a text view, as its copy has it.
    fn drawn(markdown: &str, cx: &mut TestAppContext) -> String {
        let state = cx.update(|cx| cx.new(|cx| TextViewState::markdown(markdown, cx)));
        cx.run_until_parked();
        state.update(cx, |state, cx| state.select_all(cx));
        state.read_with(cx, |state, _| state.selected_text())
    }

    /// A line break is a hard break, and a blank line, blank as Markdown
    /// has it, the end of a paragraph.
    #[test]
    fn plain_text_keeps_its_line_breaks_as_hard_breaks() {
        assert_eq!(
            plain_text_as_markdown("mismatched types\r\nexpected u32 \n\nhelp: …\n"),
            "mismatched types  \nexpected u32  \n  \nhelp\\: …"
        );
        assert_eq!(plain_text_as_markdown("one line"), "one line");
    }

    /// A problem's message draws as it was sent, whatever in it Markdown or
    /// HTML would take for its own: jdtls's `List<String>` drew as `List`,
    /// the type argument taken for an HTML tag and dropped.
    #[gpui::test]
    fn plain_text_draws_as_it_is_written(cx: &mut TestAppContext) {
        cx.update(crate::init);
        for message in [
            "Type mismatch: cannot convert from List<String> to int",
            "expected enum `Option<T>`, found `&str`",
            "a*b*c and _x_ in [a](b) & &amp; \\ trailing\\",
            "# not a heading",
            "- not a list",
            "1. not a list either",
            "> not a quote",
            "| not | a table |",
            "<b>not bold</b> https://example.com ~~kept~~",
        ] {
            assert_eq!(
                drawn(&plain_text_as_markdown(message), cx).trim_end(),
                message
            );
        }
        // its lines stay lines, and their indentation stays, where four
        // spaces would have been a block of code
        assert_eq!(
            drawn(
                &plain_text_as_markdown(
                    "    Type '{ a: number; }' is not assignable\n  Property `b` is missing"
                ),
                cx
            )
            .trim_end(),
            "\u{a0}\u{a0}\u{a0}\u{a0}Type '{ a: number; }' is not assignable\n\
             \u{a0}\u{a0}Property `b` is missing"
        );
    }

    /// A hover draws its code with its lines, its plain text as it is and
    /// its Markdown as Markdown.
    #[gpui::test]
    fn a_hover_draws_its_code_and_plain_text_as_they_are(cx: &mut TestAppContext) {
        use lsp_types::{HoverContents, LanguageString, MarkedString, MarkupContent, MarkupKind};
        cx.update(crate::init);
        let code = |value: &str| {
            MarkedString::LanguageString(LanguageString {
                language: "java".into(),
                value: value.into(),
            })
        };
        let hover = HoverContents::Scalar(code("@Override\npublic char charAt(int index)"));
        assert_eq!(
            drawn(&hover_markdown(hover), cx).trim_end(),
            "@Override\npublic char charAt(int index)"
        );
        // a fence in the code does not close the code's own
        let hover = HoverContents::Scalar(code("```\nString s = \"`\";"));
        assert_eq!(
            drawn(&hover_markdown(hover), cx).trim_end(),
            "```\nString s = \"`\";"
        );
        let hover = HoverContents::Markup(MarkupContent {
            kind: MarkupKind::PlainText,
            value: "List<String> names\n  the names".into(),
        });
        assert_eq!(
            drawn(&hover_markdown(hover), cx).trim_end(),
            "List<String> names\n\u{a0}\u{a0}the names"
        );
        let hover = HoverContents::Array(vec![
            code("int size()"),
            MarkedString::String("Returns **the number**\nof elements.".into()),
        ]);
        assert_eq!(
            drawn(&hover_markdown(hover), cx).trim_end(),
            "int size()\nReturns the number of elements."
        );
    }
}
