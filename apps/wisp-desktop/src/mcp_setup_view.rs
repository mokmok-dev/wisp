//! MCP setup window opened from the native application menu.

use std::net::SocketAddr;
use std::sync::Arc;

use gpui::{
    App, Bounds, ClipboardItem, Context, FontWeight, IntoElement, ParentElement, Render, Styled,
    TitlebarOptions, Window, WindowBounds, WindowHandle, WindowOptions, div, prelude::*, px, rgb,
    size,
};

use crate::app::AppModel;

pub struct McpSetupView {
    app: gpui::Entity<AppModel>,
    on_set_local_mcp_enabled: Arc<dyn Fn(bool, &mut App)>,
}

impl Render for McpSetupView {
    fn render(
        &mut self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let local_mcp = self.app.read(cx).local_mcp.clone();
        let command_path = local_mcp.command_path.clone();
        let ipc_token = crate::ipc_server::env_token();
        let client_configs = client_configs(&command_path, &local_mcp.addr, ipc_token.as_deref());

        div()
            .id("mcp-setup-scroll")
            .flex()
            .flex_col()
            .size_full()
            .overflow_y_scroll()
            .bg(rgb(0x0b_0e13))
            .text_color(rgb(0xe8_eaed))
            .p_6()
            .gap_5()
            .child(render_header())
            .child(render_bridge_step(
                &local_mcp,
                ipc_token.is_some(),
                self.on_set_local_mcp_enabled.clone(),
            ))
            .child(render_client_step(command_path, client_configs))
            .child(div().flex_grow())
            .child(render_footer())
    }
}

fn render_header() -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .child(
            div()
                .text_xl()
                .font_weight(FontWeight::SEMIBOLD)
                .child("Set up Wisp MCP"),
        )
        .child(
            div()
                .text_sm()
                .text_color(rgb(0x8a_8f98))
                .child("Connect an MCP client to the transcript currently visible in Wisp."),
        )
}

fn render_bridge_step(
    local_mcp: &crate::app::LocalMcpBridge,
    has_token: bool,
    on_set_enabled: Arc<dyn Fn(bool, &mut App)>,
) -> impl IntoElement {
    let (mut status, mut status_color) = if let Some(error) = &local_mcp.error {
        (format!("Failed: {error}"), rgb(0xff_5959))
    } else if local_mcp.running {
        ("Running".to_owned(), rgb(0x74_b9ff))
    } else if local_mcp.enabled {
        ("Enabled, not running".to_owned(), rgb(0x8a_8f98))
    } else {
        ("Off".to_owned(), rgb(0x5c_606b))
    };
    if !has_token && !is_loopback_addr(&local_mcp.addr) {
        status.push_str(" · Warning: non-loopback IPC has no token");
        status_color = rgb(0xff_5959);
    }
    let (action_label, should_enable) = if local_mcp.running {
        ("Disable Bridge", false)
    } else if local_mcp.enabled {
        ("Retry Bridge", true)
    } else {
        ("Enable Bridge", true)
    };

    div()
        .flex()
        .items_center()
        .justify_between()
        .gap_4()
        .p_4()
        .bg(rgb(0x13_171f))
        .border_1()
        .border_color(rgb(0x1f_242e))
        .rounded_md()
        .child(
            div()
                .flex()
                .flex_col()
                .min_w_0()
                .flex_grow()
                .gap_1()
                .child(
                    div()
                        .font_weight(FontWeight::MEDIUM)
                        .child("1. Enable the local bridge"),
                )
                .child(
                    div()
                        .text_xs()
                        .whitespace_normal()
                        .text_color(rgb(0x8a_8f98))
                        .child(format!("{status} · IPC: {}", local_mcp.addr)),
                ),
        )
        .child(
            div()
                .id("mcp-setup-toggle")
                .flex_shrink_0()
                .px_3()
                .py_1p5()
                .bg(rgb(0x18_2738))
                .border_1()
                .border_color(status_color)
                .rounded_md()
                .text_sm()
                .font_weight(FontWeight::MEDIUM)
                .cursor_pointer()
                .child(action_label)
                .on_click(move |_, _, cx| on_set_enabled(should_enable, cx)),
        )
}

fn is_loopback_addr(addr: &str) -> bool {
    addr.parse::<SocketAddr>()
        .is_ok_and(|addr| addr.ip().is_loopback())
        || addr
            .rsplit_once(':')
            .is_some_and(|(host, _)| host.eq_ignore_ascii_case("localhost"))
}

fn render_client_step(
    command_path: String,
    configs: ClientConfigs,
) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_3()
        .child(
            div()
                .font_weight(FontWeight::MEDIUM)
                .child("2. Register the Wisp stdio server"),
        )
        .child(
            div()
                .text_sm()
                .text_color(rgb(0x8a_8f98))
                .child("Add a server named “wisp” to your client. Copy a ready-to-paste config below, or use the executable path with another client:"),
        )
        .child(
            div()
                .p_3()
                .bg(rgb(0x13_171f))
                .border_1()
                .border_color(rgb(0x1f_242e))
                .rounded_md()
                .text_xs()
                .whitespace_normal()
                .line_clamp(2)
                .line_height(px(18.0))
                .child(command_path.clone()),
        )
        .child(
            div()
                .flex()
                .flex_wrap()
                .gap_2()
                .child(render_copy_button(
                    "mcp-copy-command",
                    "Copy Executable Path",
                    command_path,
                ))
                .child(render_copy_button(
                    "mcp-copy-claude-json",
                    "Copy Claude JSON",
                    configs.claude,
                ))
                .child(render_copy_button(
                    "mcp-copy-opencode-json",
                    "Copy OpenCode JSON",
                    configs.opencode,
                )),
        )
        .child(
            div()
                .text_xs()
                .whitespace_normal()
                .line_height(px(18.0))
                .text_color(rgb(0x5c_606b))
                .child("Keep Wisp running while the client is connected. The copied configs include the current IPC address and WISP_IPC_TOKEN, when set. For another client, set those environment variables yourself."),
        )
}

fn render_copy_button(
    id: &'static str,
    label: &'static str,
    value: String,
) -> impl IntoElement {
    div()
        .id(id)
        .px_3()
        .py_1p5()
        .bg(rgb(0x13_171f))
        .border_1()
        .border_color(rgb(0x1f_242e))
        .rounded_md()
        .text_sm()
        .cursor_pointer()
        .child(label)
        .on_click(move |_, _, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(value.clone()));
        })
}

fn render_footer() -> impl IntoElement {
    div().flex().justify_end().child(
        div()
            .id("mcp-setup-done")
            .px_3()
            .py_1p5()
            .bg(rgb(0x13_171f))
            .border_1()
            .border_color(rgb(0x1f_242e))
            .rounded_md()
            .cursor_pointer()
            .child("Done")
            .on_click(|_, window, _| window.remove_window()),
    )
}

pub fn open(
    cx: &mut App,
    app: gpui::Entity<AppModel>,
    on_set_local_mcp_enabled: Arc<dyn Fn(bool, &mut App)>,
) -> WindowHandle<McpSetupView> {
    let bounds = Bounds::centered(None, size(px(600.0), px(520.0)), cx);
    cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: Some(TitlebarOptions {
                title: Some("MCP Setup".into()),
                ..Default::default()
            }),
            ..Default::default()
        },
        move |_, cx| {
            cx.new(|cx| {
                cx.observe(&app, |_, _, cx| cx.notify()).detach();
                McpSetupView {
                    app,
                    on_set_local_mcp_enabled,
                }
            })
        },
    )
    .expect("failed to open MCP setup window")
}

struct ClientConfigs {
    claude: String,
    opencode: String,
}

fn client_configs(
    command_path: &str,
    ipc_addr: &str,
    ipc_token: Option<&str>,
) -> ClientConfigs {
    let mut environment = serde_json::json!({ "WISP_IPC_ADDR": ipc_addr });
    if let Some(token) = ipc_token {
        environment["WISP_IPC_TOKEN"] = token.into();
    }

    let claude = serde_json::json!({
        "mcpServers": {
            "wisp": {
                "command": command_path,
                "env": environment.clone()
            }
        }
    });
    let opencode = serde_json::json!({
        "mcp": {
            "wisp": {
                "type": "local",
                "command": [command_path],
                "environment": environment,
                "enabled": true
            }
        }
    });

    ClientConfigs {
        claude: pretty_json(&claude),
        opencode: pretty_json(&opencode),
    }
}

fn pretty_json(value: &serde_json::Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_configs_contain_command_address_and_token() {
        let command_path = r#"/Applications/Wisp "Preview".app/Contents/MacOS/wisp-mcp"#;
        let configs = client_configs(command_path, "127.0.0.1:9001", Some("secret"));
        let claude: serde_json::Value =
            serde_json::from_str(&configs.claude).expect("valid Claude JSON config");
        let opencode: serde_json::Value =
            serde_json::from_str(&configs.opencode).expect("valid OpenCode JSON config");

        assert_eq!(claude["mcpServers"]["wisp"]["command"], command_path);
        assert_eq!(
            claude["mcpServers"]["wisp"]["env"]["WISP_IPC_ADDR"],
            "127.0.0.1:9001"
        );
        assert_eq!(
            claude["mcpServers"]["wisp"]["env"]["WISP_IPC_TOKEN"],
            "secret"
        );
        assert_eq!(opencode["mcp"]["wisp"]["command"][0], command_path);
        assert_eq!(
            opencode["mcp"]["wisp"]["environment"]["WISP_IPC_ADDR"],
            "127.0.0.1:9001"
        );
        assert_eq!(
            opencode["mcp"]["wisp"]["environment"]["WISP_IPC_TOKEN"],
            "secret"
        );
    }

    #[test]
    fn client_configs_omit_missing_token() {
        let configs = client_configs("/Applications/Wisp.app/wisp-mcp", "127.0.0.1:8765", None);
        let claude: serde_json::Value =
            serde_json::from_str(&configs.claude).expect("valid Claude JSON config");

        assert!(
            claude["mcpServers"]["wisp"]["env"]
                .get("WISP_IPC_TOKEN")
                .is_none()
        );
    }

    #[test]
    fn loopback_address_detection_is_conservative() {
        assert!(is_loopback_addr("127.0.0.1:8765"));
        assert!(is_loopback_addr("[::1]:8765"));
        assert!(is_loopback_addr("localhost:8765"));
        assert!(!is_loopback_addr("0.0.0.0:8765"));
        assert!(!is_loopback_addr("192.168.1.10:8765"));
    }
}
