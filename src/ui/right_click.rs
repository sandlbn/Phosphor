use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer;
use iced::advanced::widget::{self, Widget};
use iced::advanced::{Clipboard, Shell};
use iced::mouse;
use iced::{Element, Event, Length, Rectangle, Size};

/// A transparent wrapper that intercepts right-click events and forwards
/// everything else to its child widget unchanged.
pub struct RightClickArea<'a, Message, Theme, Renderer> {
    child: Element<'a, Message, Theme, Renderer>,
    on_right_click: Box<dyn Fn(f32, f32) -> Message + 'a>,
}

impl<'a, Message, Theme, Renderer> RightClickArea<'a, Message, Theme, Renderer>
where
    Message: Clone + 'a,
    Renderer: renderer::Renderer + 'a,
    Theme: 'a,
{
    /// Wrap `child`. When the user right-clicks inside the widget bounds,
    /// `on_right_click(abs_x, abs_y)` is called to produce the message to emit.
    /// The coordinates are absolute screen pixels (same space as window position).
    pub fn new(
        child: impl Into<Element<'a, Message, Theme, Renderer>>,
        on_right_click: impl Fn(f32, f32) -> Message + 'a,
    ) -> Self {
        Self {
            child: child.into(),
            on_right_click: Box::new(on_right_click),
        }
    }
}

impl<'a, Message, Theme, Renderer> Widget<Message, Theme, Renderer>
    for RightClickArea<'a, Message, Theme, Renderer>
where
    Message: Clone + 'a,
    Renderer: renderer::Renderer + 'a,
    Theme: 'a,
{
    fn size(&self) -> Size<Length> {
        // Delegate size entirely to the child — we are zero-overhead.
        self.child.as_widget().size()
    }

    // iced 0.14: layout takes &mut self
    fn layout(
        &mut self,
        tree: &mut widget::Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.child
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }

    fn draw(
        &self,
        tree: &widget::Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        // Delegate drawing entirely — we add no visual chrome.
        self.child.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
    }

    fn children(&self) -> Vec<widget::Tree> {
        vec![widget::Tree::new(&self.child)]
    }

    fn diff(&self, tree: &mut widget::Tree) {
        tree.diff_children(std::slice::from_ref(&self.child));
    }

    // iced 0.14: on_event is NOT part of the Widget trait.
    // Instead, override update() which is the 0.14 equivalent.
    fn update(
        &mut self,
        tree: &mut widget::Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        // Forward the event to the child first so normal left-click still works.
        self.child.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );

        // Intercept right-button release inside our bounds.
        if let Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Right)) = event {
            if let Some(pos) = cursor.position_in(layout.bounds()) {
                // Convert widget-local position to absolute screen coordinates.
                let abs_x = layout.bounds().x + pos.x;
                let abs_y = layout.bounds().y + pos.y;
                shell.publish((self.on_right_click)(abs_x, abs_y));
            }
        }
    }

    fn mouse_interaction(
        &self,
        tree: &widget::Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        // Delegate cursor style to the child (e.g. pointer over a button).
        self.child.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }

    // iced 0.14: operate takes &mut self
    fn operate(
        &mut self,
        tree: &mut widget::Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn widget::Operation,
    ) {
        self.child
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
    }
}

impl<'a, Message, Theme, Renderer> From<RightClickArea<'a, Message, Theme, Renderer>>
    for Element<'a, Message, Theme, Renderer>
where
    Message: Clone + 'a,
    Renderer: renderer::Renderer + 'a,
    Theme: 'a,
{
    fn from(widget: RightClickArea<'a, Message, Theme, Renderer>) -> Self {
        Element::new(widget)
    }
}
