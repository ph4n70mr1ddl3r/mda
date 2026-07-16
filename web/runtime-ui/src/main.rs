use leptos::*;
use mda_runtime_ui::App;

fn main() {
    console_error_panic_hook::set_once();
    mount_to_body(App);
}
