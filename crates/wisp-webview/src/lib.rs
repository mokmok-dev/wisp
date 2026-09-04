//! `WebView` support for GPUI, based on [wry](https://github.com/tauri-apps/wry).
//!
//! Vendored from `longbridge/gpui-kit`'s `gpui-wry` 0.6.0 crate
//! (`crates/webview`, Apache-2.0) and adapted to depend on the official
//! `gpui` crate instead of Longbridge's `gpui-pre` snapshot. The GPUI API
//! surface this bridge uses is identical between the two.
//!
//! Behaviour notes inherited from upstream:
//!
//!   * The native webview renders on top of the GPUI window; GPUI elements
//!     behind the webview bounds are covered. Prefer using it as the whole
//!     window content or in a dedicated region.
//!   * All [`WebViewHandle`]s must be dropped before the parent window is
//!     destroyed.

use std::ops::Deref;
use std::rc::Rc;

use gpui::{
    App, Bounds, ContentMask, DismissEvent, Element, ElementId, Entity, EventEmitter, FocusHandle,
    Focusable, GlobalElementId, Hitbox, InteractiveElement, IntoElement, LayoutId, MouseDownEvent,
    ParentElement as _, Pixels, Render, Size, Style, Styled as _, Window, canvas, div,
};
use wry::{
    Rect,
    dpi::{self, LogicalSize},
};

// Re-exports so dependent crates share one wry version with this bridge.
pub use wry;

/// An owned, UI-thread-local handle to the raw wry webview.
///
/// Cloning this handle prolongs the native webview's lifetime. Dropping the
/// owning [`WebView`] entity hides the child view, but final native
/// destruction waits until all handle and frame clones are dropped. All
/// handles must be dropped before the parent window is destroyed.
#[derive(Clone)]
pub struct WebViewHandle(Rc<wry::WebView>);

impl WebViewHandle {
    /// Get the raw wry webview.
    #[must_use]
    pub fn raw(&self) -> &wry::WebView {
        &self.0
    }
}

/// A webview based on wry `WebView`.
///
/// [experimental]
pub struct WebView {
    focus_handle: FocusHandle,
    webview: Rc<wry::WebView>,
    visible: bool,
    bounds: Bounds<Pixels>,
}

impl Drop for WebView {
    fn drop(&mut self) {
        self.hide();
    }
}

impl WebView {
    /// Create a new `WebView` from a wry `WebView`.
    pub fn new(
        webview: wry::WebView,
        _: &mut Window,
        cx: &mut App,
    ) -> Self {
        let _ = webview.set_bounds(Rect::default());

        Self {
            focus_handle: cx.focus_handle(),
            visible: true,
            bounds: Bounds::default(),
            webview: Rc::new(webview),
        }
    }

    /// Show the webview.
    pub fn show(&mut self) {
        let _ = self.webview.set_visible(true);
        self.visible = true;
    }

    /// Hide the webview.
    pub fn hide(&mut self) {
        let _ = self.webview.focus_parent();
        let _ = self.webview.set_visible(false);
        self.visible = false;
    }

    /// Get whether the webview is visible.
    #[must_use]
    pub const fn visible(&self) -> bool {
        self.visible
    }

    /// Get the current bounds of the webview.
    #[must_use]
    pub const fn bounds(&self) -> Bounds<Pixels> {
        self.bounds
    }

    /// Go back in the webview history.
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying script evaluation fails.
    pub fn back(&mut self) -> anyhow::Result<()> {
        Ok(self.webview.evaluate_script("history.back();")?)
    }

    /// Load a URL in the webview.
    pub fn load_url(
        &mut self,
        url: &str,
    ) {
        let _ = self.webview.load_url(url);
    }

    /// Get the raw wry webview.
    #[must_use]
    pub fn raw(&self) -> &wry::WebView {
        &self.webview
    }

    /// Get an owned, UI-thread-local handle to the raw wry webview.
    #[must_use]
    pub fn handle(&self) -> WebViewHandle {
        WebViewHandle(self.webview.clone())
    }
}

impl Deref for WebView {
    type Target = wry::WebView;

    fn deref(&self) -> &Self::Target {
        &self.webview
    }
}

impl Focusable for WebView {
    fn focus_handle(
        &self,
        _cx: &gpui::App,
    ) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<DismissEvent> for WebView {}

impl Render for WebView {
    fn render(
        &mut self,
        _window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) -> impl IntoElement {
        let view = cx.entity();

        div()
            .track_focus(&self.focus_handle)
            .size_full()
            .child({
                let view = cx.entity();
                canvas(
                    move |bounds, _, cx| view.update(cx, |r, _| r.bounds = bounds),
                    |_, (), _, _| {},
                )
                .absolute()
                .size_full()
            })
            .child(WebViewElement::new(self.webview.clone(), view))
    }
}

/// A webview element can display a wry webview.
pub struct WebViewElement {
    parent: Entity<WebView>,
    view: Rc<wry::WebView>,
}

impl WebViewElement {
    /// Create a new webview element from a wry `WebView`.
    #[must_use]
    pub const fn new(
        view: Rc<wry::WebView>,
        parent: Entity<WebView>,
    ) -> Self {
        Self { parent, view }
    }
}

impl IntoElement for WebViewElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for WebViewElement {
    type RequestLayoutState = ();
    type PrepaintState = Option<Hitbox>;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let style = Style {
            size: Size::full(),
            flex_shrink: 1.,
            ..Default::default()
        };

        // If the parent view is no longer visible, we don't need to layout
        // the webview.
        let id = window.request_layout(style, [], cx);
        (id, ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        (): &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        if !self.parent.read(cx).visible() {
            return None;
        }

        let _ = self.view.set_bounds(Rect {
            size: dpi::Size::Logical(LogicalSize {
                width: bounds.size.width.into(),
                height: bounds.size.height.into(),
            }),
            position: dpi::Position::Logical(dpi::LogicalPosition::new(
                bounds.origin.x.into(),
                bounds.origin.y.into(),
            )),
        });

        // Create a hitbox to handle mouse event
        Some(window.insert_hitbox(bounds, gpui::HitboxBehavior::Normal))
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        (): &mut Self::RequestLayoutState,
        hitbox: &mut Self::PrepaintState,
        window: &mut Window,
        _: &mut App,
    ) {
        let bounds = hitbox.as_ref().map_or(bounds, |h| h.bounds);
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            let webview = self.view.clone();
            window.on_mouse_event(move |event: &MouseDownEvent, _, _, _| {
                if !bounds.contains(&event.position) {
                    // Click white space to blur the input focus
                    let _ = webview.focus_parent();
                }
            });
        });
    }
}
