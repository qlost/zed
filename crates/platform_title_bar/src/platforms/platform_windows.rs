use gpui::{Hsla, Rgba, WindowControlArea, prelude::*, svg};

use ui::prelude::*;

#[derive(IntoElement)]
pub struct WindowsWindowControls {
    button_height: Pixels,
}

impl WindowsWindowControls {
    pub fn new(button_height: Pixels) -> Self {
        Self { button_height }
    }

    #[cfg(not(target_os = "windows"))]
    fn get_font() -> &'static str {
        "Segoe Fluent Icons"
    }

    #[cfg(target_os = "windows")]
    fn get_font() -> &'static str {
        use windows::Wdk::System::SystemServices::RtlGetVersion;

        let mut version = unsafe { std::mem::zeroed() };
        let status = unsafe { RtlGetVersion(&mut version) };

        if status.is_ok() && version.dwBuildNumber >= 22000 {
            "Segoe Fluent Icons"
        } else {
            "Segoe MDL2 Assets"
        }
    }
}

impl RenderOnce for WindowsWindowControls {
    fn render(self, window: &mut Window, _: &mut App) -> impl IntoElement {
        div()
            .id("windows-window-controls")
            .font_family(Self::get_font())
            .flex()
            .flex_row()
            .justify_center()
            .content_stretch()
            .max_h(self.button_height)
            .min_h(self.button_height)
            .child(WindowsCaptionButton::Minimize)
            .map(|this| {
                this.child(if window.is_maximized() {
                    WindowsCaptionButton::Restore
                } else {
                    WindowsCaptionButton::Maximize
                })
            })
            .child(WindowsCaptionButton::Close)
    }
}

#[derive(IntoElement)]
enum WindowsCaptionButton {
    Minimize,
    Restore,
    Maximize,
    Close,
}

impl WindowsCaptionButton {
    #[inline]
    fn id(&self) -> &'static str {
        match self {
            Self::Minimize => "minimize",
            Self::Restore => "restore",
            Self::Maximize => "maximize",
            Self::Close => "close",
        }
    }

    // On Win7 the buttons use SVG icons instead, so these glyph strings are unused there.
    #[cfg_attr(target_vendor = "win7", allow(dead_code))]
    #[inline]
    fn icon(&self) -> &'static str {
        match self {
            Self::Minimize => "\u{e921}",
            Self::Restore => "\u{e923}",
            Self::Maximize => "\u{e922}",
            Self::Close => "\u{e8bb}",
        }
    }

    #[inline]
    fn control_area(&self) -> WindowControlArea {
        match self {
            Self::Close => WindowControlArea::Close,
            Self::Maximize | Self::Restore => WindowControlArea::Max,
            Self::Minimize => WindowControlArea::Min,
        }
    }
}

impl RenderOnce for WindowsCaptionButton {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let enabled = match &self {
            Self::Minimize => window.is_minimizable(),
            Self::Restore | Self::Maximize => window.is_resizable(),
            Self::Close => true,
        };
        let (hover_bg, hover_fg, active_bg, active_fg) = match &self {
            Self::Close => {
                let color: Hsla = Rgba {
                    r: 232.0 / 255.0,
                    g: 17.0 / 255.0,
                    b: 32.0 / 255.0,
                    a: 1.0,
                }
                .into();

                (
                    color,
                    gpui::white(),
                    color.opacity(0.8),
                    gpui::white().opacity(0.8),
                )
            }
            _ => (
                cx.theme().colors().ghost_element_hover,
                cx.theme().colors().text,
                cx.theme().colors().ghost_element_active,
                cx.theme().colors().text,
            ),
        };

        // On Win7 neither `Segoe MDL2 Assets` (Win10+) nor `Segoe Fluent Icons` (Win11+) is
        // installed, so the PUA window-control glyphs would render as missing-glyph boxes (the Win7
        // text backend has no font fallback). Draw the buttons as SVG icons instead, like the Linux
        // title-bar path does; the icon recolor on hover uses a named group on the container.
        #[cfg(target_vendor = "win7")]
        let icon = {
            let icon_name = match &self {
                Self::Minimize => IconName::GenericMinimize,
                Self::Restore => IconName::GenericRestore,
                Self::Maximize => IconName::GenericMaximize,
                Self::Close => IconName::GenericClose,
            };
            svg()
                .size_4()
                .flex_none()
                .path(icon_name.path())
                .text_color(if enabled {
                    cx.theme().colors().text
                } else {
                    cx.theme().colors().text_disabled
                })
                .when(enabled, |this| {
                    this.group_hover("", |this| this.text_color(hover_fg))
                })
                .into_any_element()
        };
        #[cfg(not(target_vendor = "win7"))]
        let icon = self.icon().into_any_element();

        let button = h_flex()
            .id(self.id())
            .justify_center()
            .content_center()
            .occlude()
            .w(px(36.))
            .h_full()
            .text_size(px(10.0))
            .when(!enabled, |style| {
                style.text_color(cx.theme().colors().text_disabled)
            })
            .when(enabled, |style| {
                style
                    .hover(|style| style.bg(hover_bg).text_color(hover_fg))
                    .active(|style| style.bg(active_bg).text_color(active_fg))
            })
            .window_control_area(self.control_area())
            .child(icon);
        // Expose a group so the Win7 SVG icon can recolor via `group_hover`.
        #[cfg(target_vendor = "win7")]
        let button = button.group("");
        button
    }
}
