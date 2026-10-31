mod assistant;
mod build_tab;
mod editor;
mod edits;
mod files_tab;
mod model_picker;
mod output_tab;
mod recipe_tab;
mod settings_window;
mod sheets;
mod terminal;
mod text_input;
mod theme;
mod widgets;
mod web_search;
mod workspace;

use gpui::{
    point, px, size, App, AppContext, Application, Bounds, KeyBinding, Menu, MenuItem, OsAction, PromptLevel, SystemMenuType, TitlebarOptions,
    WindowBounds, WindowHandle, WindowOptions,
};
use theme::AppSettings;
use workspace::*;

struct MainWindow(WindowHandle<Workspace>);
impl gpui::Global for MainWindow {}

struct SettingsWindow(Option<gpui::AnyWindowHandle>);
impl gpui::Global for SettingsWindow {}

fn bind_keys(cx: &mut App) {
    text_input::bind_keys(cx);
    editor::bind_keys(cx);
    cx.bind_keys([
        KeyBinding::new("cmd-e", files_tab::EditFile, Some("Files")),
        KeyBinding::new("cmd-n", NewProject, None),
        KeyBinding::new("cmd-o", OpenProject, None),
        KeyBinding::new("cmd-b", StartBuild, None),
        KeyBinding::new("cmd-.", CancelBuild, None),
        KeyBinding::new("cmd-1", ShowRecipe, None),
        KeyBinding::new("cmd-2", ShowBuild, None),
        KeyBinding::new("cmd-3", ShowOutput, None),
        KeyBinding::new("cmd-4", ShowFiles, None),
        KeyBinding::new("cmd-shift-t", ToggleToml, None),
        KeyBinding::new("cmd-shift-a", ToggleAssistant, None),
        KeyBinding::new("cmd-f", SearchLog, None),
        KeyBinding::new("cmd-shift-s", OpenShell, None),
        KeyBinding::new("cmd-w", CloseShell, Some("Terminal")),
        KeyBinding::new("cmd-,", OpenSettings, None),
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-w", CloseWindow, None),
        KeyBinding::new("cmd-m", Minimize, None),
        KeyBinding::new("cmd-0", ShowMainWindow, None),
        KeyBinding::new("cmd-h", HideApp, None),
        KeyBinding::new("cmd-alt-h", HideOthers, None),
    ]);
}

fn set_menus(cx: &mut App) {
    cx.set_menus(vec![
        Menu {
            name: "ImageSmith".into(),
            items: vec![
                MenuItem::action("Settings…", OpenSettings),
                MenuItem::separator(),
                MenuItem::os_submenu("Services", SystemMenuType::Services),
                MenuItem::separator(),
                MenuItem::action("Hide ImageSmith", HideApp),
                MenuItem::action("Hide Others", HideOthers),
                MenuItem::action("Show All", ShowAll),
                MenuItem::separator(),
                MenuItem::action("Quit ImageSmith", Quit),
            ],
        },
        Menu {
            name: "File".into(),
            items: vec![
                MenuItem::action("New Project…", NewProject),
                MenuItem::action("Open Project…", OpenProject),
                MenuItem::separator(),
                MenuItem::action("Close Window", CloseWindow),
            ],
        },
        Menu {
            name: "Edit".into(),
            items: vec![
                MenuItem::os_action("Cut", text_input::Cut, OsAction::Cut),
                MenuItem::os_action("Copy", text_input::Copy, OsAction::Copy),
                MenuItem::os_action("Paste", text_input::Paste, OsAction::Paste),
                MenuItem::os_action("Select All", text_input::SelectAll, OsAction::SelectAll),
            ],
        },
        Menu {
            name: "View".into(),
            items: vec![
                MenuItem::action("Recipe", ShowRecipe),
                MenuItem::action("Build", ShowBuild),
                MenuItem::action("Output", ShowOutput),
                MenuItem::action("Files", ShowFiles),
                MenuItem::separator(),
                MenuItem::action("Toggle TOML View", ToggleToml),
                MenuItem::action("Toggle Assistant", ToggleAssistant),
                MenuItem::action("Search Log", SearchLog),
            ],
        },
        Menu {
            name: "Build".into(),
            items: vec![
                MenuItem::action("Build", StartBuild),
                MenuItem::action("Cancel Build", CancelBuild),
                MenuItem::separator(),
                MenuItem::action("Open Shell", OpenShell),
            ],
        },
        Menu {
            name: "Window".into(),
            items: vec![
                MenuItem::action("Minimize", Minimize),
                MenuItem::action("Zoom", Zoom),
                MenuItem::separator(),
                MenuItem::action("ImageSmith", ShowMainWindow),
            ],
        },
    ]);
}

fn open_main_window(cx: &mut App) {
    let bounds = Bounds::centered(None, size(px(1280.), px(800.)), cx);
    let handle = cx
        .open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(960.), px(600.))),
                titlebar: Some(TitlebarOptions {
                    title: Some("ImageSmith".into()),
                    appears_transparent: true,
                    traffic_light_position: Some(point(px(14.), px(16.))),
                }),
                app_id: Some("dev.imagesmith.ImageSmith".into()),
                ..Default::default()
            },
            |window, cx| {
                let ws = cx.new(|cx| Workspace::new(window, cx));
                let fh = ws.read(cx).focus.clone();
                window.focus(&fh);
                ws
            },
        )
        .expect("open main window");
    cx.set_global(MainWindow(handle));
}

/// Brings the main window forward, reopening it if it was closed.
fn show_main_window(cx: &mut App) {
    let alive = cx.try_global::<MainWindow>().map(|m| m.0).is_some_and(|m| m.update(cx, |_, w, _| w.activate_window()).is_ok());
    if !alive {
        open_main_window(cx);
    }
    cx.activate(true);
}

fn open_settings(cx: &mut App) {
    if let Some(h) = cx.global::<SettingsWindow>().0 {
        if h.update(cx, |_, w, _| w.activate_window()).is_ok() {
            return;
        }
    }
    let bounds = Bounds::centered(None, size(px(760.), px(520.)), cx);
    let h = cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(520.), px(380.))),
            titlebar: Some(TitlebarOptions { title: Some("Settings".into()), appears_transparent: true, traffic_light_position: Some(point(px(14.), px(14.))) }),
            ..Default::default()
        },
        |window, cx| cx.new(|cx| settings_window::SettingsView::new(window, cx)),
    );
    if let Ok(h) = h {
        cx.set_global(SettingsWindow(Some(h.into())));
    }
}

/// Quit, confirming and stopping a running build or card write first.
fn quit(cx: &mut App) {
    let Some(main) = cx.try_global::<MainWindow>().map(|m| m.0) else {
        cx.quit();
        return;
    };
    let building = main.read(cx).map(|ws| ws.live.is_some()).unwrap_or(false);
    let flashing = main.read(cx).map(|ws| ws.is_flashing()).unwrap_or(false);
    if !building && !flashing {
        cx.quit();
        return;
    }
    let (title, detail, confirm) = if flashing {
        ("A card is being written", "Quitting now leaves the card unusable.", "Stop Writing and Quit")
    } else {
        ("A build is running", "Quitting cancels it.", "Cancel Build and Quit")
    };
    let _ = main.update(cx, |_, window, cx| {
        let rx = window.prompt(PromptLevel::Warning, title, Some(detail), &[confirm, "Keep Going"], cx);
        cx.spawn(async move |this, cx| {
            if rx.await.ok() != Some(0) {
                return;
            }
            let _ = this.update(cx, |ws, cx| {
                ws.queue.clear();
                ws.cancel_build(cx);
                ws.cancel_flash(cx);
            });
            for _ in 0..150 {
                cx.background_executor().timer(std::time::Duration::from_millis(100)).await;
                if this.update(cx, |ws, _| ws.live.is_none() && !ws.is_flashing()).unwrap_or(true) {
                    break;
                }
            }
            let _ = cx.update(|cx| cx.quit());
        })
        .detach();
    });
}

fn main() {
    // `flash` re-enters this binary as a privileged writer; that invocation
    // must never open a window.
    let args: Vec<String> = std::env::args().collect();
    if imagesmith_core::flash::is_helper_invocation(&args) {
        std::process::exit(imagesmith_core::flash::helper_main(&args));
    }
    let app = Application::new();
    app.on_reopen(show_main_window);
    app.run(|cx: &mut App| {
        cx.set_global(AppSettings(imagesmith_core::settings::Settings::load()));
        cx.set_global(SettingsWindow(None));
        bind_keys(cx);
        set_menus(cx);
        cx.on_action(|_: &Quit, cx| quit(cx));
        cx.on_action(|_: &OpenSettings, cx| open_settings(cx));
        cx.on_action(|_: &ShowMainWindow, cx| show_main_window(cx));
        cx.on_action(|_: &HideApp, cx| cx.hide());
        cx.on_action(|_: &HideOthers, cx| cx.hide_other_apps());
        cx.on_action(|_: &ShowAll, cx| cx.unhide_other_apps());
        cx.on_action(|_: &Minimize, cx| {
            if let Some(w) = cx.active_window() {
                let _ = w.update(cx, |_, w, _| w.minimize_window());
            }
        });
        cx.on_action(|_: &Zoom, cx| {
            if let Some(w) = cx.active_window() {
                let _ = w.update(cx, |_, w, _| w.zoom_window());
            }
        });
        cx.on_action(|_: &CloseWindow, cx| {
            if let Some(w) = cx.active_window() {
                let is_main = cx.try_global::<MainWindow>().is_some_and(|m| m.0.window_id() == w.window_id());
                if is_main {
                    quit(cx);
                } else {
                    let _ = w.update(cx, |_, w, _| w.remove_window());
                }
            }
        });
        open_main_window(cx);
        cx.activate(true);
    });
}
