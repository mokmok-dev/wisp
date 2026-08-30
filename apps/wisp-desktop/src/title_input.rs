//! Reusable single-line text input for session titles.
//!
//! The app has no external component library, so the field is implemented
//! directly on top of GPUI's `InputHandler` machinery. That gives real
//! `NSTextInputClient` integration: Japanese IMEs compose marked (undecided)
//! text through `set_marked_text` and receive it here as
//! [`TitleEditor::replace_and_mark`]. The composing range is painted with an
//! underline and the IME candidate window is positioned near the caret via
//! [`gpui::InputHandler::bounds_for_range`].
//!
//! The editing logic lives in [`TitleEditor`], which is platform-independent
//! and unit-testable. [`TitleInput`] is a thin `Element` that owns focus,
//! painting (text, caret, IME underline) and the input-handler plumbing.
//! Element identity — `FocusHandle` plus edited text — is kept in a shared
//! `Rc<RefCell<TitleInputState>>` so a field keeps its focus and caret across
//! re-renders even though the `Element` is rebuilt every frame.

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;

use gpui::{
    App, AvailableSpace, Bounds, CursorStyle, DispatchPhase, Element, ElementId, EntityId,
    FocusHandle, Hitbox, HitboxBehavior, Hsla, IntoElement, KeyBinding, KeyContext, LayoutId,
    MouseDownEvent, Pixels, Point, SharedString, Style, TextRun, TextStyle, UTF16Selection,
    UnderlineStyle, Window, actions, fill, point, px, size,
};

/// Keyboard context for the title input. Keybindings that apply inside the
/// field (Enter / Escape / arrows / backspace) are scoped to it so they never
/// shadow the application menu's global shortcuts.
const KEY_CONTEXT: &str = "wisp-title-input";

actions!(
    wisp_title,
    [
        SubmitTitle,
        CancelTitle,
        Backspace,
        DeleteForward,
        MoveLeft,
        MoveRight,
        MoveToStart,
        MoveToEnd,
    ]
);

/// Register the input's scoped keybindings. Call once at application setup.
pub fn init(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("enter", SubmitTitle, Some(KEY_CONTEXT)),
        KeyBinding::new("escape", CancelTitle, Some(KEY_CONTEXT)),
        KeyBinding::new("backspace", Backspace, Some(KEY_CONTEXT)),
        KeyBinding::new("delete", DeleteForward, Some(KEY_CONTEXT)),
        KeyBinding::new("left", MoveLeft, Some(KEY_CONTEXT)),
        KeyBinding::new("right", MoveRight, Some(KEY_CONTEXT)),
        KeyBinding::new("home", MoveToStart, Some(KEY_CONTEXT)),
        KeyBinding::new("end", MoveToEnd, Some(KEY_CONTEXT)),
    ]);
}

/// Per-field state that must survive re-renders: the edited text, the
/// `FocusHandle`, and a cache of the last caret rect (used to answer IME
/// position queries that arrive outside the paint pass).
pub struct TitleInputState {
    pub editor: TitleEditor,
    pub focus_handle: FocusHandle,
    /// Set when the field should grab keyboard focus on its next paint
    /// (used when an inline rename row first appears).
    pub request_focus: bool,
    caret_bounds: Option<Bounds<Pixels>>,
}

impl TitleInputState {
    /// Create editing state for `text` with a fresh `FocusHandle`.
    #[must_use]
    pub fn new(
        cx: &mut App,
        text: &str,
    ) -> Self {
        Self {
            editor: TitleEditor::new(text),
            focus_handle: cx.focus_handle(),
            request_focus: false,
            caret_bounds: None,
        }
    }

    /// Replace the edited text wholesale, moving the caret to the end and
    /// dropping any IME composition. Used when seeding from the model.
    pub fn set_text(
        &mut self,
        text: &str,
    ) {
        self.editor.set_text(text);
        self.caret_bounds = None;
    }

    fn set_caret_bounds(
        &mut self,
        bounds: Bounds<Pixels>,
    ) {
        self.caret_bounds = Some(bounds);
    }

    fn caret_bounds(&self) -> Option<Bounds<Pixels>> {
        self.caret_bounds
    }
}

/// How the input sizes itself within its parent.
#[derive(Debug, Clone, Copy)]
pub struct TitleInputStyle {
    /// Whether the field expands to fill the available horizontal space.
    pub fill: bool,
    pub min_width: Pixels,
    pub max_width: Option<Pixels>,
    pub pad_x: Pixels,
    pub pad_y: Pixels,
    pub font_size: Pixels,
}

impl Default for TitleInputStyle {
    fn default() -> Self {
        Self {
            fill: false,
            min_width: px(96.0),
            max_width: Some(px(320.0)),
            pad_x: px(8.0),
            pad_y: px(4.0),
            font_size: px(14.0),
        }
    }
}

/// Shape a single line of text using the current window text style.
fn shaped_line(
    window: &mut Window,
    text: &str,
    font_size: Pixels,
) -> gpui::ShapedLine {
    let text_style = window.text_style();
    let run = text_style.to_run(text.len());
    let runs = [run];
    window
        .text_system()
        .shape_line(SharedString::from(text.to_owned()), font_size, &runs, None)
}

/// A single-line, IME-aware text field element.
#[derive(Clone)]
pub struct TitleInput {
    id: ElementId,
    state: Rc<RefCell<TitleInputState>>,
    placeholder: SharedString,
    style: TitleInputStyle,
    text_color: Hsla,
    placeholder_color: Hsla,
    caret_color: Hsla,
    view_entity_id: EntityId,
    on_change: Rc<dyn Fn(&str, &mut Window, &mut App) + 'static>,
    on_commit: Rc<dyn Fn(&str, &mut Window, &mut App) + 'static>,
    on_cancel: Rc<dyn Fn(&mut Window, &mut App) + 'static>,
}

impl TitleInput {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: ElementId,
        state: Rc<RefCell<TitleInputState>>,
        view_entity_id: EntityId,
        placeholder: impl Into<SharedString>,
        style: TitleInputStyle,
        text_color: Hsla,
        placeholder_color: Hsla,
        caret_color: Hsla,
        on_change: impl Fn(&str, &mut Window, &mut App) + 'static,
        on_commit: impl Fn(&str, &mut Window, &mut App) + 'static,
        on_cancel: impl Fn(&mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            id,
            state,
            placeholder: placeholder.into(),
            style,
            text_color,
            placeholder_color,
            caret_color,
            view_entity_id,
            on_change: Rc::new(on_change),
            on_commit: Rc::new(on_commit),
            on_cancel: Rc::new(on_cancel),
        }
    }

    fn notify(
        &self,
        cx: &mut App,
    ) {
        cx.notify(self.view_entity_id);
    }

    /// The string to paint: the edited text, or the placeholder when empty.
    fn display_text(&self) -> (String, bool) {
        let state = self.state.borrow();
        let text = state.editor.text();
        if text.is_empty() {
            (self.placeholder.to_string(), true)
        } else {
            (text.to_string(), false)
        }
    }

    /// Text runs for painting: base runs plus an underline over the IME
    /// composing range.
    fn runs_for(
        &self,
        text: &str,
        is_placeholder: bool,
        base: &TextStyle,
    ) -> Vec<TextRun> {
        if is_placeholder {
            return vec![base.to_run(text.len())];
        }
        let Some(marked) = self.state.borrow().editor.marked() else {
            return vec![base.to_run(text.len())];
        };
        let start = marked.start.min(text.len());
        let end = marked.end.clamp(start, text.len());
        let mut runs = Vec::with_capacity(3);
        if start > 0 {
            runs.push(base.to_run(start));
        }
        if end > start {
            let mut marked_style = base.clone();
            marked_style.underline = Some(UnderlineStyle {
                thickness: px(1.0),
                color: Some(self.text_color),
                wavy: false,
            });
            runs.push(marked_style.to_run(end - start));
        }
        if end < text.len() {
            runs.push(base.to_run(text.len() - end));
        }
        runs
    }

    /// Map a window x-coordinate to a caret byte index.
    fn caret_index_for_x(
        window: &mut Window,
        text: &str,
        style: TitleInputStyle,
        bounds_origin_x: Pixels,
        x: Pixels,
    ) -> usize {
        if text.is_empty() {
            return 0;
        }
        let content_x = (x - bounds_origin_x - style.pad_x).max(px(0.0));
        let line = shaped_line(window, text, style.font_size);
        let index = line
            .index_for_x(content_x)
            .unwrap_or(line.len())
            .min(line.len());
        text.floor_char_boundary(index)
    }

    fn commit_editing(
        &self,
        window: &mut Window,
        cx: &mut App,
    ) {
        let text = self.state.borrow().editor.text().to_string();
        (self.on_commit)(&text, window, cx);
    }
}

impl gpui::InputHandler for TitleInput {
    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<UTF16Selection> {
        let editor = &self.state.borrow().editor;
        let range = editor.selected_text_range_utf16();
        Some(UTF16Selection {
            range,
            reversed: false,
        })
    }

    fn marked_text_range(
        &mut self,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<Range<usize>> {
        self.state.borrow().editor.marked_text_range_utf16()
    }

    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<String> {
        if range_utf16.end < range_utf16.start {
            return None;
        }
        let editor = &self.state.borrow().editor;
        let len = byte_to_utf16_offset(editor.text(), editor.text().len());
        *adjusted_range = Some(range_utf16.start..len.min(range_utf16.end));
        Some(editor.text_for_range_utf16(range_utf16.start, range_utf16.end))
    }

    fn replace_text_in_range(
        &mut self,
        replacement_range: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut App,
    ) {
        {
            let mut state = self.state.borrow_mut();
            state
                .editor
                .replace_range(replacement_range.as_ref().map(|r| (r.start, r.end)), text);
        }
        (self.on_change)(text, window, cx);
        self.notify(cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut App,
    ) {
        {
            let mut state = self.state.borrow_mut();
            state.editor.replace_and_mark(
                range_utf16.as_ref().map(|r| (r.start, r.end)),
                new_text,
                new_selected_range.as_ref().map(|r| (r.start, r.end)),
            );
        }
        (self.on_change)(new_text, window, cx);
        self.notify(cx);
    }

    fn unmark_text(
        &mut self,
        _window: &mut Window,
        cx: &mut App,
    ) {
        self.state.borrow_mut().editor.unmark();
        self.notify(cx);
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<Bounds<Pixels>> {
        // IME candidate placement queries can arrive outside the paint pass,
        // so they can't read this element's current bounds. Report the last
        // caret rect painted this frame.
        self.state.borrow().caret_bounds()
    }

    fn character_index_for_point(
        &mut self,
        point: Point<Pixels>,
        window: &mut Window,
        _cx: &mut App,
    ) -> Option<usize> {
        let text = self.state.borrow().editor.text().to_string();
        let style = self.style;
        let byte_index = Self::caret_index_for_x(window, &text, style, px(0.0), point.x);
        Some(byte_to_utf16_offset(&text, byte_index))
    }
}

impl IntoElement for TitleInput {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for TitleInput {
    type RequestLayoutState = ();
    type PrepaintState = Hitbox;

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&gpui::GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        _cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let (text, _) = self.display_text();
        let style = self.style;
        let layout_id = window.request_measured_layout(
            Style::default(),
            move |_known, available, window, _cx| {
                let line = shaped_line(window, &text, style.font_size);
                let text_width = line.width;
                let line_height = (line.ascent + line.descent).max(px(1.0));
                let mut width = text_width + style.pad_x * 2.0;
                if style.fill {
                    if let AvailableSpace::Definite(available_width) = available.width {
                        let cap = style.max_width.unwrap_or(available_width);
                        width = width.max(available_width.min(cap));
                    }
                } else if let Some(max_width) = style.max_width {
                    width = width.min(max_width);
                }
                width = width.max(style.min_width);
                size(width, line_height + style.pad_y * 2.0)
            },
        );
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&gpui::GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        {
            let state = self.state.borrow();
            window.set_focus_handle(&state.focus_handle, cx);
        }
        window.insert_hitbox(bounds, HitboxBehavior::BlockMouse)
    }

    #[allow(clippy::too_many_lines)] // imperative paint: focus, hitbox, IME, caret
    fn paint(
        &mut self,
        _id: Option<&gpui::GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let hitbox = prepaint.clone();
        window.set_cursor_style(CursorStyle::IBeam, &hitbox);

        let style = self.style;
        let focused = {
            let state = self.state.borrow();
            state.focus_handle.is_focused(window)
        };

        // --- mouse interaction -----------------------------------------------
        let state_for_mouse = self.state.clone();
        let on_commit = self.on_commit.clone();
        let view_entity_id = self.view_entity_id;
        window.on_mouse_event(
            move |event: &MouseDownEvent, phase: DispatchPhase, window, cx| {
                if phase != DispatchPhase::Bubble {
                    return;
                }
                if bounds.contains(&event.position) {
                    let was_focused = {
                        let state = state_for_mouse.borrow();
                        state.focus_handle.is_focused(window)
                    };
                    let text = state_for_mouse.borrow().editor.text().to_string();
                    let caret = TitleInput::caret_index_for_x(
                        window,
                        &text,
                        style,
                        bounds.origin.x,
                        event.position.x,
                    );
                    {
                        let mut state = state_for_mouse.borrow_mut();
                        state.request_focus = false;
                        if !was_focused {
                            state.focus_handle.focus(window);
                        }
                        state.editor.set_cursor(caret);
                    }
                    cx.notify(view_entity_id);
                } else {
                    let commit_on_blur = {
                        let state = state_for_mouse.borrow();
                        state.focus_handle.is_focused(window) && !state.editor.text().is_empty()
                    };
                    if commit_on_blur {
                        let text = state_for_mouse.borrow().editor.text().to_string();
                        on_commit(&text, window, cx);
                    }
                }
            },
        );

        // --- keyboard actions (dispatched only when this node is focused) -----
        {
            let mut key_context = KeyContext::default();
            key_context.add(KEY_CONTEXT);
            window.set_key_context(key_context);
        }

        {
            let this = self.clone();
            window.on_action(
                std::any::TypeId::of::<SubmitTitle>(),
                move |_: &dyn std::any::Any, _: DispatchPhase, window, cx| {
                    if this.state.borrow().focus_handle.is_focused(window) {
                        this.commit_editing(window, cx);
                    }
                },
            );
        }
        {
            let this = self.clone();
            window.on_action(
                std::any::TypeId::of::<CancelTitle>(),
                move |_: &dyn std::any::Any, _: DispatchPhase, window, cx| {
                    if this.state.borrow().focus_handle.is_focused(window) {
                        (this.on_cancel)(window, cx);
                    }
                },
            );
        }
        let mut register_edit_action = |action_type: std::any::TypeId, op: fn(&mut TitleEditor)| {
            let this = self.clone();
            window.on_action(
                action_type,
                move |_: &dyn std::any::Any, _: DispatchPhase, window, cx| {
                    if !this.state.borrow().focus_handle.is_focused(window) {
                        return;
                    }
                    op(&mut this.state.borrow_mut().editor);
                    this.notify(cx);
                },
            );
        };
        register_edit_action(std::any::TypeId::of::<Backspace>(), TitleEditor::backspace);
        register_edit_action(
            std::any::TypeId::of::<DeleteForward>(),
            TitleEditor::delete_forward,
        );
        register_edit_action(std::any::TypeId::of::<MoveLeft>(), TitleEditor::move_left);
        register_edit_action(std::any::TypeId::of::<MoveRight>(), TitleEditor::move_right);
        register_edit_action(
            std::any::TypeId::of::<MoveToStart>(),
            TitleEditor::move_to_start,
        );
        register_edit_action(
            std::any::TypeId::of::<MoveToEnd>(),
            TitleEditor::move_to_end,
        );

        // Focus once when the field first appears (fresh inline-rename row),
        // before registering the input handler so the next keystroke is not
        // dropped while the focus change propagates.
        if self.state.borrow().request_focus {
            let focus_handle = self.state.borrow().focus_handle.clone();
            {
                let mut state = self.state.borrow_mut();
                state.request_focus = false;
            }
            focus_handle.focus(window);
        }

        // --- text input plumbing ----------------------------------------------
        {
            let state = self.state.borrow();
            window.handle_input(&state.focus_handle, self.clone(), cx);
        }

        // --- painting ----------------------------------------------------------
        let (text, is_placeholder) = self.display_text();
        let mut base = window.text_style().clone();
        base.color = if is_placeholder {
            self.placeholder_color
        } else {
            self.text_color
        };
        let runs = self.runs_for(&text, is_placeholder, &base);
        let shaped = window.text_system().shape_line(
            SharedString::from(text.clone()),
            style.font_size,
            &runs,
            None,
        );
        let line_height = (shaped.ascent + shaped.descent).max(px(1.0));
        let text_origin = point(bounds.origin.x + style.pad_x, bounds.origin.y + style.pad_y);
        shaped.paint(text_origin, line_height, window, cx).ok();

        if focused {
            let caret_byte = self.state.borrow().editor.cursor().min(text.len());
            let caret_x = if text.is_empty() {
                px(0.0)
            } else {
                shaped.x_for_index(caret_byte).min(shaped.width)
            };
            let caret_bounds = Bounds::new(
                point(text_origin.x + caret_x, bounds.origin.y + style.pad_y),
                size(px(1.5), line_height),
            );
            window.paint_quad(fill(caret_bounds, self.caret_color));
            self.state.borrow_mut().set_caret_bounds(caret_bounds);
        }
    }
}

// --- pure editing logic -------------------------------------------------------

/// Platform-independent single-line text editing state.
///
/// The caret is a single byte offset. The IME composition (marked) range is
/// stored as a byte range. All platform-facing mutations accept UTF-16 ranges
/// (as exposed by [`gpui::InputHandler`]) and translate them to byte offsets,
/// respecting surrogate pairs.
#[derive(Debug, Clone)]
pub struct TitleEditor {
    text: String,
    /// Caret position in bytes.
    cursor: usize,
    /// Byte range of the IME-composing text, present while composing.
    marked: Option<Range<usize>>,
}

/// Replace line-break characters so pasted multi-line text stays on one line.
fn sanitize_newlines(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch == '\n' || ch == '\r' {
            out.push(' ');
        } else {
            out.push(ch);
        }
    }
    out
}

impl TitleEditor {
    /// Create an editor seeded with `text`, caret at the end.
    #[must_use]
    pub fn new(text: &str) -> Self {
        let text = sanitize_newlines(text);
        let cursor = text.len();
        Self {
            text,
            cursor,
            marked: None,
        }
    }

    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    #[must_use]
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    #[must_use]
    pub fn marked(&self) -> Option<Range<usize>> {
        self.marked.clone()
    }

    /// Replace the whole buffer, caret to end, dropping IME state.
    pub fn set_text(
        &mut self,
        text: &str,
    ) {
        self.text = sanitize_newlines(text);
        self.cursor = self.text.len();
        self.marked = None;
    }

    /// Place the caret at a byte offset (snapped to a char boundary).
    pub fn set_cursor(
        &mut self,
        byte_index: usize,
    ) {
        self.cursor = self
            .text
            .floor_char_boundary(byte_index.min(self.text.len()));
    }

    /// Replace a UTF-16 range or, when `range` is `None`, the caret with the
    /// given committed text. Maps to `insertText:`.
    pub fn replace_range(
        &mut self,
        range: Option<(usize, usize)>,
        text: &str,
    ) {
        let text = sanitize_newlines(text);
        let (start, end) = self.resolve_range(range);
        self.text.replace_range(start..end, &text);
        self.cursor = start + text.len();
        self.marked = None;
    }

    /// Replace a UTF-16 range and mark the replacement as composing text,
    /// placing the caret at `selected` (a UTF-16 offset within `text`).
    /// Maps to `setMarkedText:`.
    pub fn replace_and_mark(
        &mut self,
        range: Option<(usize, usize)>,
        text: &str,
        selected: Option<(usize, usize)>,
    ) {
        let text = sanitize_newlines(text);
        let (start, end) = self.resolve_range(range);
        self.text.replace_range(start..end, &text);
        let marked_end = start + text.len();
        self.marked = Some(start..marked_end);
        self.cursor = match selected {
            Some((sel_start, _)) => start + byte_of_utf16_offset(&text, sel_start),
            None => marked_end,
        }
        .min(self.text.len());
    }

    /// Drop the IME composing state.
    pub fn unmark(&mut self) {
        self.marked = None;
    }

    pub fn backspace(&mut self) {
        if let Some(marked) = self.marked.clone() {
            let start = clamp_boundary(&self.text, marked.start);
            let end = clamp_boundary(&self.text, marked.end);
            self.text.replace_range(start..end, "");
            self.cursor = start;
            self.marked = None;
            return;
        }
        if self.cursor == 0 {
            return;
        }
        let start = self.prev_boundary(self.cursor);
        self.text.replace_range(start..self.cursor, "");
        self.cursor = start;
    }

    pub fn delete_forward(&mut self) {
        if let Some(marked) = self.marked.clone() {
            let start = clamp_boundary(&self.text, marked.start);
            let end = clamp_boundary(&self.text, marked.end);
            self.text.replace_range(start..end, "");
            self.cursor = start;
            self.marked = None;
            return;
        }
        if self.cursor >= self.text.len() {
            return;
        }
        let end = self.next_boundary(self.cursor);
        self.text.replace_range(self.cursor..end, "");
    }

    pub fn move_left(&mut self) {
        if let Some(marked) = self.marked.clone()
            && marked.start < marked.end
            && self.cursor >= marked.start
            && self.cursor <= marked.end
            && self.cursor != marked.start
        {
            // Inside the composition, or at its trailing boundary: the
            // first arrow step cancels the composition selection.
            self.cursor = marked.start;
            return;
        }
        self.cursor = self.prev_boundary(self.cursor);
    }

    pub fn move_right(&mut self) {
        if let Some(marked) = self.marked.clone()
            && marked.start < marked.end
            && self.cursor >= marked.start
            && self.cursor < marked.end
        {
            self.cursor = marked.end;
            return;
        }
        self.cursor = self.next_boundary(self.cursor);
    }

    pub fn move_to_start(&mut self) {
        self.cursor = self.marked.as_ref().map_or(0, |marked| marked.start);
    }

    pub fn move_to_end(&mut self) {
        self.cursor = self
            .marked
            .as_ref()
            .map_or(self.text.len(), |marked| marked.end);
    }

    fn prev_boundary(
        &self,
        byte_index: usize,
    ) -> usize {
        if byte_index == 0 {
            return 0;
        }
        self.text[..byte_index]
            .chars()
            .next_back()
            .map_or(0, |ch| byte_index - ch.len_utf8())
    }

    fn next_boundary(
        &self,
        byte_index: usize,
    ) -> usize {
        byte_index
            + self.text[byte_index..]
                .chars()
                .next()
                .map_or(0, char::len_utf8)
    }

    /// Translate an optional UTF-16 range into a clamped byte range.
    fn resolve_range(
        &self,
        range: Option<(usize, usize)>,
    ) -> (usize, usize) {
        let (start_utf16, end_utf16) = match range {
            Some((start, end)) => (start, end),
            None => (
                byte_to_utf16_offset(&self.text, self.cursor),
                byte_to_utf16_offset(&self.text, self.cursor),
            ),
        };
        let start = byte_of_utf16_offset(&self.text, start_utf16);
        let end = byte_of_utf16_offset(&self.text, end_utf16);
        let (start, end) = (start.min(end), start.max(end));
        (
            clamp_boundary(&self.text, start),
            clamp_boundary(&self.text, end),
        )
    }

    /// The selected range as UTF-16 offsets (a caret is a zero-width range).
    #[must_use]
    pub fn selected_text_range_utf16(&self) -> Range<usize> {
        let index = byte_to_utf16_offset(&self.text, self.cursor);
        index..index
    }

    /// The IME composing range as UTF-16 offsets, if composing.
    #[must_use]
    pub fn marked_text_range_utf16(&self) -> Option<Range<usize>> {
        self.marked.as_ref().map(|range| {
            byte_to_utf16_offset(&self.text, range.start)
                ..byte_to_utf16_offset(&self.text, range.end)
        })
    }

    /// Substring for a UTF-16 range.
    #[must_use]
    pub fn text_for_range_utf16(
        &self,
        start_utf16: usize,
        end_utf16: usize,
    ) -> String {
        let start = byte_of_utf16_offset(&self.text, start_utf16);
        let end = byte_of_utf16_offset(&self.text, end_utf16);
        let (start, end) = (start.min(end), start.max(end));
        self.text[start..end].to_string()
    }
}

fn clamp_boundary(
    text: &str,
    byte_index: usize,
) -> usize {
    text.floor_char_boundary(byte_index.min(text.len()))
}

/// Convert a UTF-16 code-unit offset into a byte offset into `text`, snapping
/// to the surrounding character boundary.
fn byte_of_utf16_offset(
    text: &str,
    utf16_index: usize,
) -> usize {
    let mut current = 0;
    for (byte_index, ch) in text.char_indices() {
        if current >= utf16_index {
            return byte_index;
        }
        let next = current + ch.len_utf16();
        if utf16_index < next {
            return byte_index + ch.len_utf8();
        }
        current = next;
    }
    text.len()
}

/// Convert a byte offset into a UTF-16 code-unit offset.
fn byte_to_utf16_offset(
    text: &str,
    byte_index: usize,
) -> usize {
    let mut count = 0;
    for (index, ch) in text.char_indices() {
        if index >= byte_index {
            break;
        }
        count += ch.len_utf16();
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backspace_deletes_previous_char() {
        let mut editor = TitleEditor::new("ab");
        editor.set_cursor(2);
        editor.backspace();
        assert_eq!(editor.text(), "a");
        assert_eq!(editor.cursor(), 1);
        editor.backspace();
        assert_eq!(editor.text(), "");
    }

    #[test]
    fn delete_forward_removes_next_char() {
        let mut editor = TitleEditor::new("ab");
        editor.set_cursor(0);
        editor.delete_forward();
        assert_eq!(editor.text(), "b");
        assert_eq!(editor.cursor(), 0);
    }

    #[test]
    fn caret_moves_across_char_boundaries() {
        let mut editor = TitleEditor::new("😀a");
        editor.set_cursor(0);
        editor.move_right();
        assert_eq!(editor.cursor(), 4);
        editor.move_right();
        assert_eq!(editor.cursor(), 5);

        editor.move_left();
        assert_eq!(editor.cursor(), 4);
        editor.move_left();
        assert_eq!(editor.cursor(), 0);
    }

    #[test]
    fn backspace_keeps_emoji_whole() {
        let mut editor = TitleEditor::new("a😀");
        editor.move_to_end();
        editor.backspace();
        assert_eq!(editor.text(), "a");
    }

    #[test]
    fn replace_range_in_utf16() {
        let mut editor = TitleEditor::new("カ会議");
        // "カ" and "会" are one UTF-16 unit each; replace the first two units.
        editor.replace_range(Some((0, 2)), "定例");
        assert_eq!(editor.text(), "定例議");
    }

    #[test]
    fn newlines_are_sanitized_to_spaces() {
        let editor = TitleEditor::new("a\nb\r\nc");
        assert_eq!(editor.text(), "a b  c");
    }

    #[test]
    fn ime_composition_marks_and_unmarks() {
        let mut editor = TitleEditor::new("会");
        // Replace the single-char buffer with composing text "かい" and place
        // the caret at its end ("かい" is 3 UTF-16 units).
        editor.replace_and_mark(Some((0, 1)), "かい", Some((3, 3)));
        assert_eq!(editor.text(), "かい");
        assert_eq!(editor.marked(), Some(0..6));
        assert_eq!(editor.cursor(), 6);

        // Commit with insertText at the marked range.
        editor.replace_range(Some((0, 3)), "会議");
        assert_eq!(editor.text(), "会議");
        assert_eq!(editor.marked(), None);
    }

    #[test]
    fn backspace_removes_whole_marked_range() {
        let mut editor = TitleEditor::new("xy");
        editor.set_cursor(1);
        editor.replace_and_mark(Some((1, 1)), "かい", Some((3, 3)));
        assert_eq!(editor.text(), "xかいy");
        editor.backspace();
        assert_eq!(editor.text(), "xy");
        assert_eq!(editor.marked(), None);
    }

    #[test]
    fn caret_arrows_respect_marked_region() {
        let mut editor = TitleEditor::new("aかいb");
        editor.set_cursor(1);
        editor.replace_and_mark(Some((1, 1)), "かい", Some((3, 3)));
        // Caret sits at the end of the marked text; left snaps to marked.start.
        editor.move_left();
        assert_eq!(editor.cursor(), 1);
        // Right from the start of the composition jumps to its end.
        editor.move_right();
        assert_eq!(editor.cursor(), 1 + "かい".len());
    }

    #[test]
    fn utf16_offsets_handle_surrogate_pairs() {
        let text = "a😀b";
        assert_eq!(byte_of_utf16_offset(text, 0), 0);
        assert_eq!(byte_of_utf16_offset(text, 1), 1);
        // "😀" occupies UTF-16 units 1..3, i.e. bytes 1..5.
        assert_eq!(byte_to_utf16_offset(text, 5), 3);
        assert_eq!(byte_of_utf16_offset(text, 3), 5);
        assert_eq!(byte_to_utf16_offset(text, text.len()), 4);
    }
}
