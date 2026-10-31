use gpui::{rgb, rgba, App, Global, Hsla, Window, WindowAppearance};
use imagesmith_core::settings::{Settings, Theme};

/// App-wide settings, shared by the main and Settings windows.
pub struct AppSettings(pub Settings);

impl Global for AppSettings {}

pub fn settings(cx: &App) -> &Settings {
    &cx.global::<AppSettings>().0
}

#[derive(Clone, Copy)]
pub struct Colors {
    pub dark: bool,
    pub bg: Hsla,
    pub sidebar: Hsla,
    pub surface: Hsla,
    pub surface_alt: Hsla,
    pub border: Hsla,
    pub text: Hsla,
    pub muted: Hsla,
    pub faint: Hsla,
    pub accent: Hsla,
    pub accent_text: Hsla,
    pub accent_soft: Hsla,
    pub hover: Hsla,
    pub selected: Hsla,
    pub success: Hsla,
    pub success_soft: Hsla,
    pub danger: Hsla,
    pub danger_soft: Hsla,
    pub warning: Hsla,
    pub warning_soft: Hsla,
    pub info_soft: Hsla,
    pub input_bg: Hsla,
    pub log_bg: Hsla,
    pub overlay: Hsla,
    pub page: Hsla,
    pub group: Hsla,
    pub separator: Hsla,
    pub track: Hsla,
}

impl Colors {
    pub fn get(window: &Window, cx: &App) -> Colors {
        let dark = match settings(cx).theme {
            Theme::Light => false,
            Theme::Dark => true,
            Theme::System => matches!(window.appearance(), WindowAppearance::Dark | WindowAppearance::VibrantDark),
        };
        if dark {
            Colors {
                dark,
                bg: rgb(0x1e1e20).into(),
                sidebar: rgb(0x252528).into(),
                surface: rgb(0x2a2a2e).into(),
                surface_alt: rgb(0x323237).into(),
                border: rgb(0x3d3d43).into(),
                text: rgb(0xe8e8ea).into(),
                muted: rgb(0xa0a0a8).into(),
                faint: rgb(0x6c6c74).into(),
                accent: rgb(0x4a8fe7).into(),
                accent_text: rgb(0xffffff).into(),
                accent_soft: rgba(0x4a8fe740).into(),
                hover: rgba(0xffffff12).into(),
                selected: rgba(0x4a8fe733).into(),
                success: rgb(0x4cc26b).into(),
                success_soft: rgba(0x4cc26b2a).into(),
                danger: rgb(0xf06a5f).into(),
                danger_soft: rgba(0xf06a5f2a).into(),
                warning: rgb(0xe8b339).into(),
                warning_soft: rgba(0xe8b3392a).into(),
                info_soft: rgba(0x4a8fe72a).into(),
                input_bg: rgb(0x1a1a1c).into(),
                log_bg: rgb(0x18181a).into(),
                overlay: rgba(0x00000099).into(),
                page: rgb(0x1e1e20).into(),
                group: rgb(0x2a2a2d).into(),
                separator: rgba(0xffffff14).into(),
                track: rgba(0xffffff1a).into(),
            }
        } else {
            Colors {
                dark,
                bg: rgb(0xffffff).into(),
                sidebar: rgb(0xf5f5f4).into(),
                surface: rgb(0xffffff).into(),
                surface_alt: rgb(0xf7f7f6).into(),
                border: rgb(0xdcdcd8).into(),
                text: rgb(0x1d1d1f).into(),
                muted: rgb(0x6b6b70).into(),
                faint: rgb(0xa8a8ac).into(),
                accent: rgb(0x2f74d0).into(),
                accent_text: rgb(0xffffff).into(),
                accent_soft: rgba(0x2f74d026).into(),
                hover: rgba(0x0000000d).into(),
                selected: rgba(0x2f74d01f).into(),
                success: rgb(0x2e9e4f).into(),
                success_soft: rgba(0x2e9e4f1f).into(),
                danger: rgb(0xd0453a).into(),
                danger_soft: rgba(0xd0453a1a).into(),
                warning: rgb(0xc08a10).into(),
                warning_soft: rgba(0xe8b33933).into(),
                info_soft: rgba(0x2f74d01a).into(),
                input_bg: rgb(0xffffff).into(),
                log_bg: rgb(0xfbfbfa).into(),
                overlay: rgba(0x00000059).into(),
                page: rgb(0xf5f5f7).into(),
                group: rgb(0xffffff).into(),
                separator: rgba(0x0000001a).into(),
                track: rgba(0x0000000f).into(),
            }
        }
    }
}

pub const MONO: &str = "Menlo";
pub const UI_FONT: &str = ".SystemUIFont";
