use gpui::{div, prelude::*, px, size, App, Application, Bounds, Context, Window, WindowBounds, WindowOptions};
struct Hello;
impl Render for Hello {
    fn render(&mut self, _w: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement { div().size_full().child("Kimono") }
}
fn main() {
    Application::new().run(|cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(400.), px(300.)), cx);
        cx.open_window(WindowOptions { window_bounds: Some(WindowBounds::Windowed(bounds)), ..Default::default() }, |_, cx| cx.new(|_| Hello)).unwrap();
    });
}
