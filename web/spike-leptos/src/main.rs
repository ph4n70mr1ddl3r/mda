//! Entry point compiled to WASM by Trunk.

use leptos::*;
use mda_spike_leptos::App;

fn main() {
    console_error_panic_hook::set_once();
    mount_to_body(App);
}
