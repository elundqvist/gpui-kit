use gpui::{Bounds, Pixels, Point, px};

/// The popovers an editor shows for what is under the pointer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PopoverKind {
    /// The language server's hover.
    Hover,
    /// A problem's message.
    Problem,
}

/// How far the pointer may stray from the nearest it has come to a
/// popover and still be on its way to it: VS Code's hover allows 4 px.
const SLACK: Pixels = px(4.);

/// The pointer's way to a popover the editor shows, so that a move that
/// takes it on toward the popover leaves the popover up, though it
/// crosses text with no problem or the room past a line's end on the way:
/// VS Code's hover stays while the pointer gets closer to it, never
/// further than the nearest it came by more than 4 px (its
/// `isMouseGettingCloser`). It went at the first such move, and its text
/// could not be picked nor its links followed (elundqvist/kvist#666).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Approach {
    /// Where the popover was drawn last.
    popover: Option<Bounds<Pixels>>,
    /// The nearest the pointer has come to it since it was drawn there.
    nearest: Pixels,
}

impl Approach {
    /// The popover drawn at `popover` over what shows it, `trigger`, which
    /// the pointer is over as it shows: drawn somewhere else, the way to
    /// it starts at the far end of the trigger.
    pub(crate) fn drawn_at(&mut self, popover: Bounds<Pixels>, trigger: Bounds<Pixels>) {
        if self.popover == Some(popover) {
            return;
        }
        self.popover = Some(popover);
        self.nearest = [
            trigger.origin,
            trigger.top_right(),
            trigger.bottom_left(),
            trigger.bottom_right(),
        ]
        .into_iter()
        .map(|corner| distance(corner, popover))
        .fold(px(0.), |far, away| far.max(away));
    }

    /// Whether the pointer at `at` is on its way to the popover: no
    /// further from it than the nearest it has come, give or take
    /// `SLACK`. Each move toward it brings the nearest in.
    pub(crate) fn heading_for(&mut self, at: Point<Pixels>) -> bool {
        let Some(popover) = self.popover else {
            return false;
        };
        let away = distance(at, popover);
        if away > self.nearest + SLACK {
            return false;
        }
        self.nearest = self.nearest.min(away);
        true
    }
}

/// How far `at` is from the nearest point of `bounds`.
fn distance(at: Point<Pixels>, bounds: Bounds<Pixels>) -> Pixels {
    let dx = f32::from(
        (bounds.left() - at.x)
            .max(at.x - bounds.right())
            .max(px(0.)),
    );
    let dy = f32::from(
        (bounds.top() - at.y)
            .max(at.y - bounds.bottom())
            .max(px(0.)),
    );
    px(dx.hypot(dy))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, size};

    /// A popover 200 px wide over a word of 20 px at its left end, and
    /// the pointer leaving the word: up and to the right across the row's
    /// other words it is on its way, and back down it is not.
    #[test]
    fn the_way_to_a_popover_is_any_move_that_takes_the_pointer_no_further_from_it() {
        let popover = Bounds::new(point(px(100.), px(60.)), size(px(200.), px(40.)));
        let trigger = Bounds::new(point(px(100.), px(100.)), size(px(20.), px(20.)));
        let mut approach = Approach::default();
        assert!(
            !approach.heading_for(point(px(110.), px(110.))),
            "no popover drawn"
        );
        approach.drawn_at(popover, trigger);
        // along the row, as far from the popover as over the word
        assert!(approach.heading_for(point(px(150.), px(115.))));
        // up and on to it
        assert!(approach.heading_for(point(px(200.), px(105.))));
        assert!(approach.heading_for(point(px(250.), px(101.))));
        // a little back, within the slack
        assert!(approach.heading_for(point(px(255.), px(104.))));
        // away, down the editor
        assert!(!approach.heading_for(point(px(255.), px(130.))));
        // drawn again where it was, the way goes on from the nearest
        approach.drawn_at(popover, trigger);
        assert!(!approach.heading_for(point(px(255.), px(115.))));
        // drawn elsewhere, it starts again at the trigger
        approach.drawn_at(popover.dilate(px(1.)), trigger);
        assert!(approach.heading_for(point(px(255.), px(115.))));
    }
}
