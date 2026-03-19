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
        self.child.as_widget().size()
    }

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
        // Use cursor.position() for hit-testing rather than cursor.is_over().
        // Inside a scrollable the layout bounds are virtual coordinates — rows
        // scrolled into view have y positions above the viewport origin, so
        // is_over() returns false even when the cursor is visually over them.
        let is_right = matches!(
            event,
            Event::Mouse(
                mouse::Event::ButtonPressed(mouse::Button::Right)
                    | mouse::Event::ButtonReleased(mouse::Button::Right)
            )
        );

        if is_right {
            let in_bounds = cursor
                .position()
                .map(|pos| layout.bounds().contains(pos))
                .unwrap_or(false);

            if in_bounds {
                if matches!(
                    event,
                    Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Right))
                ) {
                    if let Some(pos) = cursor.position() {
                        shell.publish((self.on_right_click)(pos.x, pos.y));
                    }
                }
                // Swallow both press and release — prevents the child button
                // from firing PlaylistSelect on right-click.
                return;
            }
        }

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
    }

    fn mouse_interaction(
        &self,
        tree: &widget::Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.child.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }

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
