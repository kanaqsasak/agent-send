#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    agent_send_desktop_lib::run();
}
