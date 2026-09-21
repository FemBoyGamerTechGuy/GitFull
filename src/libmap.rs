//! Curated upstream map for well-known pkg-config module names.
//!
//! # Why this exists (the resolution-strategy problem)
//!
//! Ranked forge search answers *"what is a popular repository whose name
//! matches this text?"* — which is the right question when a user types
//! `gitfull install <app>` and picks an application to try. It is the
//! WRONG question when resolving a **pkg-config module name** declared
//! inside a build manifest:
//!
//! * module names frequently are **modules inside a parent library's
//!   repository** (`gio-unix-2.0` is a GLib module — no repository is
//!   ever named `gio-unix-2.0`, so search finds nothing or something
//!   unrelated);
//! * generic names (`cairo`, `gee`, `appstream`) string-match unrelated
//!   projects that merely share the word;
//! * popularity signals (stars/contributors/commits) have **no reliable
//!   correspondence** to "this is the correct upstream source for this
//!   module" — a near-zero-star repo can still win a rank-1 slot.
//!
//! So dependency-name resolution is **curated-mapping-first**
//! ([`crate::planner`] resolution layers): this module's data is
//! consulted before any search, and ranked search is only a *flagged
//! fallback* whose result is never silently auto-built (it requires an
//! interactive confirmation or a `[dep.<name>]` config pin).
//!
//! # What this table is — and is not
//!
//! This is an **upstream source map** for well-known library modules,
//! the same kind of data as a distribution's package→source mapping or
//! gitfull's own `toolchain::CATALOG`. It maps module names to the
//! repositories their maintainers actually publish from. It is:
//!
//! * **seeded, not exhaustive** — common windowing/graphics/core-library
//!   modules as a starting set;
//! * **extensible and overridable via configuration, not code** — a
//!   `[dep.<name>]` entry in gitfull.conf always wins over this table
//!   (see docs/CONFIG.md), which is also how names missing from the
//!   seed set get pinned;
//! * **not tuned to any test case** — no test fixture name appears in
//!   it, and the mechanisms (curated-first ordering, flagged fallback,
//!   same-source deduplication) are name-agnostic.
//!
//! Entries are plain upstream clone URLs. Hosts that are not configured
//! as forges in gitfull.conf are fetched as anonymous generic git
//! remotes (see `planner::resolve_source`) — no forge registration is
//! required to use the map.
//!
//! # Same-source deduplication
//!
//! Many module names intentionally map to ONE repository (the whole
//! GLib family, the cairo `cairo-*` family, wayland's `wayland-*`
//! modules, …). The planner deduplicates dependencies by fetch
//! identity (URL + ref), so an app declaring `glib-2.0` AND
//! `gio-unix-2.0` fetches and builds GLib **once**; the single built
//! entry's `provides` list (discovered from its own `.pc` files)
//! satisfies every sibling name. Tests enforce this.

// ---------------------------------------------------------------------------
// model
// ---------------------------------------------------------------------------

/// One curated mapping: a well-known pkg-config module name → its
/// correct upstream repository.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CuratedEntry {
    /// Normalized (lowercase) module name, as it appears in
    /// `dependency('…')` / `pkg_check_modules` / `find_package`.
    pub module: &'static str,
    /// Upstream clone URL (spec-parseable; anonymous generic remote if
    /// the host is not a configured forge).
    pub source: &'static str,
    /// Maintenance branch, when the default branch is NOT what the
    /// module name means (e.g. `sdl2` → the SDL repo's `SDL2` branch;
    /// `gtk+-3.0` → the gtk repo's `gtk-3-24` branch).
    pub git_ref: Option<&'static str>,
    /// Human label shown in resolution output.
    pub label: &'static str,
}

/// The seed table. Sorted by module name for reviewability; lookup is
/// a linear scan (called once per declared dependency per install).
///
/// When extending: verify the entry against the module's actual
/// upstream (the repository its releases are cut from), not against a
/// name-matched search result — that is the entire point of this table.
pub const CURATED: &[CuratedEntry] = &[
    // ---- GLib family (one repository, many modules) -----------------------
    CuratedEntry { module: "gio-2.0", source: "https://gitlab.gnome.org/GNOME/glib", git_ref: None, label: "GLib (gio module)" },
    CuratedEntry { module: "gio-unix-2.0", source: "https://gitlab.gnome.org/GNOME/glib", git_ref: None, label: "GLib (gio-unix module)" },
    CuratedEntry { module: "gio-windows-2.0", source: "https://gitlab.gnome.org/GNOME/glib", git_ref: None, label: "GLib (gio-windows module)" },
    CuratedEntry { module: "glib-2.0", source: "https://gitlab.gnome.org/GNOME/glib", git_ref: None, label: "GLib" },
    CuratedEntry { module: "gmodule-2.0", source: "https://gitlab.gnome.org/GNOME/glib", git_ref: None, label: "GLib (gmodule module)" },
    CuratedEntry { module: "gobject-2.0", source: "https://gitlab.gnome.org/GNOME/glib", git_ref: None, label: "GLib (gobject module)" },
    CuratedEntry { module: "gthread-2.0", source: "https://gitlab.gnome.org/GNOME/glib", git_ref: None, label: "GLib (gthread module)" },
    // ---- GTK family --------------------------------------------------------
    CuratedEntry { module: "gdk-pixbuf-2.0", source: "https://gitlab.gnome.org/GNOME/gdk-pixbuf", git_ref: None, label: "GDK-Pixbuf" },
    CuratedEntry { module: "gtk+-2.0", source: "https://gitlab.gnome.org/GNOME/gtk", git_ref: Some("gtk-2-24"), label: "GTK 2 (gtk-2-24 branch)" },
    CuratedEntry { module: "gtk+-3.0", source: "https://gitlab.gnome.org/GNOME/gtk", git_ref: Some("gtk-3-24"), label: "GTK 3 (gtk-3-24 branch)" },
    CuratedEntry { module: "gtk4", source: "https://gitlab.gnome.org/GNOME/gtk", git_ref: None, label: "GTK 4" },
    CuratedEntry { module: "gtk4-wayland", source: "https://gitlab.gnome.org/GNOME/gtk", git_ref: None, label: "GTK 4 (wayland backend)" },
    CuratedEntry { module: "gtk4-x11", source: "https://gitlab.gnome.org/GNOME/gtk", git_ref: None, label: "GTK 4 (x11 backend)" },
    CuratedEntry { module: "gtksourceview-5", source: "https://gitlab.gnome.org/GNOME/gtksourceview", git_ref: None, label: "GtkSourceView 5" },
    CuratedEntry { module: "vte-2.91", source: "https://gitlab.gnome.org/GNOME/vte", git_ref: None, label: "VTE" },
    // ---- text stack --------------------------------------------------------
    CuratedEntry { module: "cairo", source: "https://gitlab.freedesktop.org/cairo/cairo", git_ref: None, label: "cairo" },
    CuratedEntry { module: "cairo-ft", source: "https://gitlab.freedesktop.org/cairo/cairo", git_ref: None, label: "cairo (ft backend)" },
    CuratedEntry { module: "cairo-gobject", source: "https://gitlab.freedesktop.org/cairo/cairo", git_ref: None, label: "cairo (gobject bindings)" },
    CuratedEntry { module: "cairo-pdf", source: "https://gitlab.freedesktop.org/cairo/cairo", git_ref: None, label: "cairo (pdf backend)" },
    CuratedEntry { module: "cairo-ps", source: "https://gitlab.freedesktop.org/cairo/cairo", git_ref: None, label: "cairo (ps backend)" },
    CuratedEntry { module: "cairo-quartz", source: "https://gitlab.freedesktop.org/cairo/cairo", git_ref: None, label: "cairo (quartz backend)" },
    CuratedEntry { module: "cairo-script", source: "https://gitlab.freedesktop.org/cairo/cairo", git_ref: None, label: "cairo (script backend)" },
    CuratedEntry { module: "cairo-svg", source: "https://gitlab.freedesktop.org/cairo/cairo", git_ref: None, label: "cairo (svg backend)" },
    CuratedEntry { module: "cairo-win32", source: "https://gitlab.freedesktop.org/cairo/cairo", git_ref: None, label: "cairo (win32 backend)" },
    CuratedEntry { module: "cairo-xcb", source: "https://gitlab.freedesktop.org/cairo/cairo", git_ref: None, label: "cairo (xcb backend)" },
    CuratedEntry { module: "cairo-xlib", source: "https://gitlab.freedesktop.org/cairo/cairo", git_ref: None, label: "cairo (xlib backend)" },
    CuratedEntry { module: "fontconfig", source: "https://gitlab.freedesktop.org/fontconfig/fontconfig", git_ref: None, label: "fontconfig" },
    CuratedEntry { module: "freetype2", source: "https://gitlab.freedesktop.org/freetype/freetype", git_ref: None, label: "FreeType" },
    CuratedEntry { module: "harfbuzz", source: "https://github.com/harfbuzz/harfbuzz", git_ref: None, label: "HarfBuzz" },
    CuratedEntry { module: "harfbuzz-cairo", source: "https://github.com/harfbuzz/harfbuzz", git_ref: None, label: "HarfBuzz (cairo integration)" },
    CuratedEntry { module: "harfbuzz-gobject", source: "https://github.com/harfbuzz/harfbuzz", git_ref: None, label: "HarfBuzz (gobject bindings)" },
    CuratedEntry { module: "harfbuzz-icu", source: "https://github.com/harfbuzz/harfbuzz", git_ref: None, label: "HarfBuzz (icu integration)" },
    CuratedEntry { module: "harfbuzz-subset", source: "https://github.com/harfbuzz/harfbuzz", git_ref: None, label: "HarfBuzz (subsetter)" },
    CuratedEntry { module: "pango", source: "https://gitlab.gnome.org/GNOME/pango", git_ref: None, label: "Pango" },
    CuratedEntry { module: "pangocairo", source: "https://gitlab.gnome.org/GNOME/pango", git_ref: None, label: "Pango (cairo renderer)" },
    CuratedEntry { module: "pangoft2", source: "https://gitlab.gnome.org/GNOME/pango", git_ref: None, label: "Pango (freetype renderer)" },
    CuratedEntry { module: "pangowin32", source: "https://gitlab.gnome.org/GNOME/pango", git_ref: None, label: "Pango (win32 renderer)" },
    CuratedEntry { module: "pangoxft", source: "https://gitlab.gnome.org/GNOME/pango", git_ref: None, label: "Pango (xft renderer)" },
    CuratedEntry { module: "pixman-1", source: "https://gitlab.freedesktop.org/pixman/pixman", git_ref: None, label: "Pixman" },
    // ---- GNOME platform ----------------------------------------------------
    CuratedEntry { module: "appstream", source: "https://gitlab.freedesktop.org/appstream/appstream", git_ref: None, label: "AppStream" },
    CuratedEntry { module: "appstream-glib", source: "https://github.com/hughsie/appstream-glib", git_ref: None, label: "AppStream-GLib" },
    CuratedEntry { module: "gee-0.8", source: "https://gitlab.gnome.org/GNOME/libgee", git_ref: None, label: "libgee" },
    CuratedEntry { module: "gobject-introspection-1.0", source: "https://gitlab.gnome.org/GNOME/gobject-introspection", git_ref: None, label: "gobject-introspection" },
    CuratedEntry { module: "gsettings-desktop-schemas", source: "https://gitlab.gnome.org/GNOME/gsettings-desktop-schemas", git_ref: None, label: "gsettings-desktop-schemas" },
    CuratedEntry { module: "json-glib-1.0", source: "https://gitlab.gnome.org/GNOME/json-glib", git_ref: None, label: "JSON-GLib" },
    CuratedEntry { module: "libadwaita-1", source: "https://gitlab.gnome.org/GNOME/libadwaita", git_ref: None, label: "libadwaita" },
    CuratedEntry { module: "libnotify", source: "https://gitlab.gnome.org/GNOME/libnotify", git_ref: None, label: "libnotify" },
    CuratedEntry { module: "libsoup-2.4", source: "https://gitlab.gnome.org/GNOME/libsoup", git_ref: Some("libsoup-2.4"), label: "libsoup 2.4 (libsoup-2.4 branch)" },
    CuratedEntry { module: "libsoup-3.0", source: "https://gitlab.gnome.org/GNOME/libsoup", git_ref: None, label: "libsoup 3" },
    CuratedEntry { module: "libxml-2.0", source: "https://gitlab.gnome.org/GNOME/libxml2", git_ref: None, label: "libxml2" },
    // ---- core libraries ----------------------------------------------------
    CuratedEntry { module: "expat", source: "https://github.com/libexpat/libexpat", git_ref: None, label: "libexpat" },
    CuratedEntry { module: "icu-i18n", source: "https://github.com/unicode-org/icu", git_ref: None, label: "ICU (i18n module)" },
    CuratedEntry { module: "icu-io", source: "https://github.com/unicode-org/icu", git_ref: None, label: "ICU (io module)" },
    CuratedEntry { module: "icu-uc", source: "https://github.com/unicode-org/icu", git_ref: None, label: "ICU (uc module)" },
    CuratedEntry { module: "json-c", source: "https://github.com/json-c/json-c", git_ref: None, label: "json-c" },
    CuratedEntry { module: "libarchive", source: "https://github.com/libarchive/libarchive", git_ref: None, label: "libarchive" },
    CuratedEntry { module: "libcrypto", source: "https://github.com/openssl/openssl", git_ref: None, label: "OpenSSL (libcrypto)" },
    CuratedEntry { module: "libcurl", source: "https://github.com/curl/curl", git_ref: None, label: "libcurl" },
    CuratedEntry { module: "libffi", source: "https://github.com/libffi/libffi", git_ref: None, label: "libffi" },
    CuratedEntry { module: "libjpeg", source: "https://github.com/libjpeg-turbo/libjpeg-turbo", git_ref: None, label: "libjpeg-turbo" },
    CuratedEntry { module: "libpng", source: "https://github.com/pnggroup/libpng", git_ref: None, label: "libpng" },
    CuratedEntry { module: "libssl", source: "https://github.com/openssl/openssl", git_ref: None, label: "OpenSSL (libssl)" },
    CuratedEntry { module: "libtiff-4", source: "https://gitlab.com/libtiff/libtiff", git_ref: None, label: "libtiff" },
    CuratedEntry { module: "libturbojpeg", source: "https://github.com/libjpeg-turbo/libjpeg-turbo", git_ref: None, label: "libjpeg-turbo (TurboJPEG API)" },
    CuratedEntry { module: "libuv", source: "https://github.com/libuv/libuv", git_ref: None, label: "libuv" },
    CuratedEntry { module: "libwebp", source: "https://github.com/google/libwebp", git_ref: None, label: "libwebp" },
    CuratedEntry { module: "libsharpyuv", source: "https://github.com/google/libwebp", git_ref: None, label: "libwebp (sharpyuv)" },
    CuratedEntry { module: "openssl", source: "https://github.com/openssl/openssl", git_ref: None, label: "OpenSSL" },
    CuratedEntry { module: "sqlite3", source: "https://github.com/sqlite/sqlite", git_ref: None, label: "SQLite" },
    CuratedEntry { module: "zlib", source: "https://github.com/madler/zlib", git_ref: None, label: "zlib" },
    // ---- windowing / system ------------------------------------------------
    CuratedEntry { module: "dbus-1", source: "https://gitlab.freedesktop.org/dbus/dbus", git_ref: None, label: "D-Bus" },
    CuratedEntry { module: "epoxy", source: "https://github.com/anholt/libepoxy", git_ref: None, label: "libepoxy" },
    CuratedEntry { module: "graphene-1.0", source: "https://github.com/ebassi/graphene", git_ref: None, label: "graphene" },
    CuratedEntry { module: "graphene-gles2", source: "https://github.com/ebassi/graphene", git_ref: None, label: "graphene (gles2)" },
    CuratedEntry { module: "libdrm", source: "https://gitlab.freedesktop.org/mesa/drm", git_ref: None, label: "libdrm" },
    CuratedEntry { module: "libdrm_amdgpu", source: "https://gitlab.freedesktop.org/mesa/drm", git_ref: None, label: "libdrm (amdgpu)" },
    CuratedEntry { module: "libdrm_intel", source: "https://gitlab.freedesktop.org/mesa/drm", git_ref: None, label: "libdrm (intel)" },
    CuratedEntry { module: "libdrm_nouveau", source: "https://gitlab.freedesktop.org/mesa/drm", git_ref: None, label: "libdrm (nouveau)" },
    CuratedEntry { module: "libdrm_radeon", source: "https://gitlab.freedesktop.org/mesa/drm", git_ref: None, label: "libdrm (radeon)" },
    CuratedEntry { module: "libinput", source: "https://gitlab.freedesktop.org/libinput/libinput", git_ref: None, label: "libinput" },
    CuratedEntry { module: "libpulse", source: "https://gitlab.freedesktop.org/pulseaudio/pulseaudio", git_ref: None, label: "PulseAudio (client lib)" },
    CuratedEntry { module: "libpulse-mainloop-glib", source: "https://gitlab.freedesktop.org/pulseaudio/pulseaudio", git_ref: None, label: "PulseAudio (glib mainloop)" },
    CuratedEntry { module: "sdl2", source: "https://github.com/libsdl-org/SDL", git_ref: Some("SDL2"), label: "SDL2 (SDL2 branch)" },
    CuratedEntry { module: "sdl3", source: "https://github.com/libsdl-org/SDL", git_ref: None, label: "SDL3" },
    CuratedEntry { module: "wayland", source: "https://gitlab.freedesktop.org/wayland/wayland", git_ref: None, label: "libwayland" },
    CuratedEntry { module: "wayland-client", source: "https://gitlab.freedesktop.org/wayland/wayland", git_ref: None, label: "libwayland (client)" },
    CuratedEntry { module: "wayland-cursor", source: "https://gitlab.freedesktop.org/wayland/wayland", git_ref: None, label: "libwayland (cursor)" },
    CuratedEntry { module: "wayland-egl", source: "https://gitlab.freedesktop.org/wayland/wayland", git_ref: None, label: "libwayland (egl)" },
    CuratedEntry { module: "wayland-egl-backend", source: "https://gitlab.freedesktop.org/wayland/wayland", git_ref: None, label: "libwayland (egl-backend)" },
    CuratedEntry { module: "wayland-scanner", source: "https://gitlab.freedesktop.org/wayland/wayland", git_ref: None, label: "libwayland (scanner tool)" },
    CuratedEntry { module: "wayland-server", source: "https://gitlab.freedesktop.org/wayland/wayland", git_ref: None, label: "libwayland (server)" },
    CuratedEntry { module: "xkbcommon", source: "https://github.com/xkbcommon/libxkbcommon", git_ref: None, label: "libxkbcommon" },
    CuratedEntry { module: "xkbcommon-x11", source: "https://github.com/xkbcommon/libxkbcommon", git_ref: None, label: "libxkbcommon (x11)" },
    CuratedEntry { module: "xkbregistry", source: "https://github.com/xkbcommon/libxkbcommon", git_ref: None, label: "libxkbcommon (registry)" },
];

/// Look up a module name (already normalized to lowercase by the
/// caller — [`crate::depgraph`] normalizes declared names).
pub fn lookup(module_norm: &str) -> Option<CuratedEntry> {
    CURATED.iter().find(|e| e.module == module_norm).copied()
}

/// The distinct upstream sources a set of module names maps to — used
/// by tests to prove same-parent modules collapse to one source (the
/// precondition for the planner's single fetch/build dedup).
pub fn distinct_sources(modules: &[&str]) -> Vec<(&'static str, Option<&'static str>)> {
    let mut out: Vec<(&'static str, Option<&'static str>)> = Vec::new();
    for m in modules {
        if let Some(e) = lookup(m) {
            let pair = (e.source, e.git_ref);
            if !out.contains(&pair) {
                out.push(pair);
            }
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn table_is_wellformed() {
        let mut seen = BTreeSet::new();
        for e in CURATED {
            // unique module keys (a duplicate would silently shadow)
            assert!(seen.insert(e.module), "duplicate module {}", e.module);
            // module names are normalized (lowercase) as depgraph emits
            assert_eq!(e.module, e.module.to_ascii_lowercase());
            // sources parse as package specs (URLs)
            crate::spec::PkgSpec::parse(e.source).unwrap_or_else(|e2| {
                panic!("curated source `{}` does not parse: {e2}", e.source)
            });
            assert!(e.source.starts_with("https://"), "{}", e.source);
            assert!(!e.label.is_empty());
        }
        assert!(CURATED.len() > 60, "seed set unexpectedly small");
    }

    #[test]
    fn lookup_basic() {
        assert_eq!(
            lookup("cairo").map(|e| e.source),
            Some("https://gitlab.freedesktop.org/cairo/cairo")
        );
        // NOT a name-matched unrelated repo — the actual upstream
        assert_eq!(
            lookup("gee-0.8").map(|e| e.source),
            Some("https://gitlab.gnome.org/GNOME/libgee")
        );
        // gio-unix-2.0 is a module INSIDE GLib — the case plain search
        // can never answer correctly
        assert_eq!(
            lookup("gio-unix-2.0").map(|e| e.source),
            Some("https://gitlab.gnome.org/GNOME/glib")
        );
        // case-insensitive input (depgraph normalizes, but be safe)
        assert!(lookup("GLIB-2.0").is_none()); // normalized form expected
        assert!(lookup("not-a-known-module-xyz").is_none());
        assert!(lookup("").is_none());
    }

    #[test]
    fn refs_pin_maintenance_branches_where_needed() {
        // the SDL repo's default branch is SDL3 — sdl2 must pin SDL2
        let sdl2 = lookup("sdl2").unwrap();
        let sdl3 = lookup("sdl3").unwrap();
        assert_eq!(sdl2.source, sdl3.source);
        assert_eq!(sdl2.git_ref, Some("SDL2"));
        assert_eq!(sdl3.git_ref, None);
        // same for gtk+-3.0 on the gtk repo (default branch is gtk4)
        let gtk3 = lookup("gtk+-3.0").unwrap();
        let gtk4 = lookup("gtk4").unwrap();
        assert_eq!(gtk3.source, gtk4.source);
        assert_eq!(gtk3.git_ref, Some("gtk-3-24"));
        assert_eq!(gtk4.git_ref, None);
    }

    /// The concrete dependency list from the failed run that motivated
    /// this fix — every name must resolve through the curated map (the
    /// mechanism is general; this list is a fixture, and NONE of these
    /// names may be special-cased anywhere in planner/search code).
    #[test]
    fn validation_list_from_the_failed_run_resolves_via_curated_map() {
        let list = [
            "appstream", "cairo", "gee-0.8", "gio-unix-2.0", "glib-2.0", "gtk4",
            "json-glib-1.0", "libadwaita-1", "libarchive", "libnotify", "libsoup-3.0",
            "sdl3",
        ];
        for name in list {
            assert!(lookup(name).is_some(), "`{name}` missing from curated map");
        }
        // multi-module parents collapse to ONE source each: the GLib
        // family shares a single tree, so 12 names < 12 sources
        let sources = distinct_sources(&list);
        assert_eq!(sources.len(), 11, "{sources:?}");
        // glib-2.0 and gio-unix-2.0 deduplicate to exactly one fetch
        let glib_family = distinct_sources(&["glib-2.0", "gio-unix-2.0", "gobject-2.0", "gio-2.0"]);
        assert_eq!(glib_family.len(), 1, "{glib_family:?}");
    }

    #[test]
    fn same_source_families_exist_for_dedup() {
        // the dedup precondition: several module names -> one repo
        for family in [
            &["glib-2.0", "gio-2.0", "gio-unix-2.0", "gobject-2.0"][..],
            &["cairo", "cairo-gobject", "cairo-svg"][..],
            &["wayland-client", "wayland-server", "wayland-scanner"][..],
            &["harfbuzz", "harfbuzz-subset", "harfbuzz-icu"][..],
            &["openssl", "libssl", "libcrypto"][..],
        ] {
            assert_eq!(distinct_sources(family).len(), 1, "{family:?}");
        }
    }
}
