//! Vendored SVG icons (Phosphor), embedded at build time.
//!
//! The dashboard serves icons from `dashboard/web/icons/` when a checkout is
//! reachable (`repo_root()`), but the installed LXC runs from `/srv/oura` with
//! no repository beside it, so every `/icons/*.svg` request 404'd and the UI
//! rendered without icons. Embedding the files keeps them working in the
//! packaged build; the on-disk copy still wins so the UI stays editable
//! without recompiling.

const ICONS: &[(&str, &[u8])] = &[
    (
        "act-cycling.svg",
        include_bytes!("../../../dashboard/web/icons/act-cycling.svg"),
    ),
    (
        "act-default.svg",
        include_bytes!("../../../dashboard/web/icons/act-default.svg"),
    ),
    (
        "act-kcal.svg",
        include_bytes!("../../../dashboard/web/icons/act-kcal.svg"),
    ),
    (
        "act-running.svg",
        include_bytes!("../../../dashboard/web/icons/act-running.svg"),
    ),
    (
        "act-strength.svg",
        include_bytes!("../../../dashboard/web/icons/act-strength.svg"),
    ),
    (
        "act-swimming.svg",
        include_bytes!("../../../dashboard/web/icons/act-swimming.svg"),
    ),
    (
        "act-walking.svg",
        include_bytes!("../../../dashboard/web/icons/act-walking.svg"),
    ),
    (
        "act-yoga.svg",
        include_bytes!("../../../dashboard/web/icons/act-yoga.svg"),
    ),
    (
        "arrow-left.svg",
        include_bytes!("../../../dashboard/web/icons/arrow-left.svg"),
    ),
    (
        "arrows-clockwise.svg",
        include_bytes!("../../../dashboard/web/icons/arrows-clockwise.svg"),
    ),
    (
        "battery-high.svg",
        include_bytes!("../../../dashboard/web/icons/battery-high.svg"),
    ),
    (
        "cpu.svg",
        include_bytes!("../../../dashboard/web/icons/cpu.svg"),
    ),
    (
        "flask.svg",
        include_bytes!("../../../dashboard/web/icons/flask.svg"),
    ),
    (
        "heartbeat.svg",
        include_bytes!("../../../dashboard/web/icons/heartbeat.svg"),
    ),
    (
        "magnifying-glass.svg",
        include_bytes!("../../../dashboard/web/icons/magnifying-glass.svg"),
    ),
    (
        "moon.svg",
        include_bytes!("../../../dashboard/web/icons/moon.svg"),
    ),
    (
        "person-simple-run.svg",
        include_bytes!("../../../dashboard/web/icons/person-simple-run.svg"),
    ),
    (
        "sun.svg",
        include_bytes!("../../../dashboard/web/icons/sun.svg"),
    ),
    (
        "test-tube.svg",
        include_bytes!("../../../dashboard/web/icons/test-tube.svg"),
    ),
    (
        "upload-simple.svg",
        include_bytes!("../../../dashboard/web/icons/upload-simple.svg"),
    ),
    (
        "user.svg",
        include_bytes!("../../../dashboard/web/icons/user.svg"),
    ),
    (
        "warning.svg",
        include_bytes!("../../../dashboard/web/icons/warning.svg"),
    ),
    (
        "wind.svg",
        include_bytes!("../../../dashboard/web/icons/wind.svg"),
    ),
];

/// Look up an embedded icon by file name, e.g. `arrows-clockwise.svg`.
pub fn get(name: &str) -> Option<&'static [u8]> {
    ICONS
        .iter()
        .find(|(file, _)| *file == name)
        .map(|(_, bytes)| *bytes)
}
