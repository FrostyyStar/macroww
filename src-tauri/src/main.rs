// Prevents an additional console window from appearing on Windows in
// release builds. Do not remove.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    macroww_lib::run();
}
