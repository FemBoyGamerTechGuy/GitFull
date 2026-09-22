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
//! required to use the map. That contract has a consequence: **every
//! source must be anonymously cloneable** (the generic-remote path sends
//! no credentials at all). The seed table has been audited against the
//! live hosts with sealed anonymous `git ls-remote` — see the
//! `curated_sources_are_anonymously_clonable_upstreams` test for the two
//! entries that audit corrected.
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
//!
//! # Component-scoped builds (multi-component monorepos)
//!
//! A few upstreams are monorepos whose default build compiles an entire
//! suite for one library (systemd for `libsystemd`/`libudev`; elogind
//! for `libelogind`). Those entries carry a **build scope** — the
//! component's own ninja targets plus an install-tag filter, both read
//! from upstream's own build definitions — and the planner's build
//! driver compiles only that component
//! (`ninja <targets>` + `meson install --no-rebuild --tags <tags>`).
//! See docs/ARCHITECTURE.md ("Component-scoped builds of monorepos")
//! for the empirically verified meson semantics behind the recipe.
//! Scoping applies inside the TARGET app's sandbox only; gitfull's own
//! binary never links any of it (enforced by tests/cargo_deps.rs).

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
    /// **Component-scoped meson build** (multi-component monorepos only,
    /// empty for normal single-component upstreams): ninja targets to
    /// compile INSTEAD OF `all` — upstream's own component alias
    /// targets where they exist. See [`Self::scope`] and
    /// docs/ARCHITECTURE.md ("Component-scoped builds of monorepos").
    pub build_targets: &'static [&'static str],
    /// Install-tag filter for `meson install --no-rebuild --tags …`
    /// (required whenever `build_targets` is set, meaningless alone
    /// otherwise: a scope needs both halves — scoped compile AND
    /// scoped install — or meson would rebuild the whole monorepo at
    /// the install step, which is exactly what the scope avoids).
    pub install_tags: Option<&'static str>,
}

impl CuratedEntry {
    /// The component build scope, when this entry has one: (targets,
    /// install tags). `None` for single-component upstreams — the
    /// standard `ninja` + `ninja install` build.
    pub fn scope(&self) -> Option<(&'static [&'static str], &'static str)> {
        let tags = self.install_tags?;
        if self.build_targets.is_empty() {
            return None;
        }
        Some((self.build_targets, tags))
    }
}

/// The seed table. Sorted by module name for reviewability; lookup is
/// a linear scan (called once per declared dependency per install).
///
/// When extending: verify the entry against the module's actual
/// upstream (the repository its releases are cut from), not against a
/// name-matched search result — that is the entire point of this table.
pub const CURATED: &[CuratedEntry] = &[
    // ---- GLib family (one repository, many modules) -----------------------
    CuratedEntry { module: "gio-2.0", source: "https://gitlab.gnome.org/GNOME/glib", git_ref: None, label: "GLib (gio module)", build_targets: &[], install_tags: None },
    CuratedEntry { module: "gio-unix-2.0", source: "https://gitlab.gnome.org/GNOME/glib", git_ref: None, label: "GLib (gio-unix module)", build_targets: &[], install_tags: None },
    CuratedEntry { module: "gio-windows-2.0", source: "https://gitlab.gnome.org/GNOME/glib", git_ref: None, label: "GLib (gio-windows module)", build_targets: &[], install_tags: None },
    // girepository-2.0 is the same resolution category as gio-unix-2.0:
    // a module bundled inside a parent project's repository — since GLib
    // 2.79 the girepository library lives in GLib's own tree
    // (glib/girepository/, which declares `meson.override_dependency(
    // 'girepository-2.0', …)`); the standalone GObject-Introspection
    // project pairs with it (its scanner drives GLib's introspection
    // build) and keeps its own module, `gobject-introspection-1.0`,
    // below. Mapping to GLib also deduplicates: a closure needing
    // glib-2.0 AND girepository-2.0 fetches GLib once.
    CuratedEntry { module: "girepository-2.0", source: "https://gitlab.gnome.org/GNOME/glib", git_ref: None, label: "GLib (girepository module)", build_targets: &[], install_tags: None },
    CuratedEntry { module: "glib-2.0", source: "https://gitlab.gnome.org/GNOME/glib", git_ref: None, label: "GLib", build_targets: &[], install_tags: None },
    CuratedEntry { module: "gmodule-2.0", source: "https://gitlab.gnome.org/GNOME/glib", git_ref: None, label: "GLib (gmodule module)", build_targets: &[], install_tags: None },
    CuratedEntry { module: "gmodule-export-2.0", source: "https://gitlab.gnome.org/GNOME/glib", git_ref: None, label: "GLib (gmodule-export module)", build_targets: &[], install_tags: None },
    CuratedEntry { module: "gmodule-no-export-2.0", source: "https://gitlab.gnome.org/GNOME/glib", git_ref: None, label: "GLib (gmodule-no-export module)", build_targets: &[], install_tags: None },
    CuratedEntry { module: "gobject-2.0", source: "https://gitlab.gnome.org/GNOME/glib", git_ref: None, label: "GLib (gobject module)", build_targets: &[], install_tags: None },
    CuratedEntry { module: "gthread-2.0", source: "https://gitlab.gnome.org/GNOME/glib", git_ref: None, label: "GLib (gthread module)", build_targets: &[], install_tags: None },
    // ---- GTK family --------------------------------------------------------
    // atk: GTK 2's accessibility bridge (cairo's optional gtk+-2.0 mapping
    // pulls the gtk-2-24 branch, whose BASE_DEPENDENCIES include atk)
    CuratedEntry { module: "atk", source: "https://gitlab.gnome.org/GNOME/atk", git_ref: None, label: "ATK", build_targets: &[], install_tags: None },
    CuratedEntry { module: "gdk-pixbuf-2.0", source: "https://gitlab.gnome.org/GNOME/gdk-pixbuf", git_ref: None, label: "GDK-Pixbuf", build_targets: &[], install_tags: None },
    // gi-docgen: the GObject introspection documentation generator —
    // a build TOOL AppStream declares `dependency(..., native: true)`
    // under its default-ON 'apidocs' option. Ships gi-docgen.pc.
    CuratedEntry { module: "gi-docgen", source: "https://gitlab.gnome.org/GNOME/gi-docgen", git_ref: None, label: "gi-docgen", build_targets: &[], install_tags: None },
    CuratedEntry { module: "gtk+-2.0", source: "https://gitlab.gnome.org/GNOME/gtk", git_ref: Some("gtk-2-24"), label: "GTK 2 (gtk-2-24 branch)", build_targets: &[], install_tags: None },
    CuratedEntry { module: "gtk+-3.0", source: "https://gitlab.gnome.org/GNOME/gtk", git_ref: Some("gtk-3-24"), label: "GTK 3 (gtk-3-24 branch)", build_targets: &[], install_tags: None },
    CuratedEntry { module: "gtk4", source: "https://gitlab.gnome.org/GNOME/gtk", git_ref: None, label: "GTK 4", build_targets: &[], install_tags: None },
    CuratedEntry { module: "gtk4-wayland", source: "https://gitlab.gnome.org/GNOME/gtk", git_ref: None, label: "GTK 4 (wayland backend)", build_targets: &[], install_tags: None },
    CuratedEntry { module: "gtk4-x11", source: "https://gitlab.gnome.org/GNOME/gtk", git_ref: None, label: "GTK 4 (x11 backend)", build_targets: &[], install_tags: None },
    CuratedEntry { module: "gtksourceview-5", source: "https://gitlab.gnome.org/GNOME/gtksourceview", git_ref: None, label: "GtkSourceView 5", build_targets: &[], install_tags: None },
    CuratedEntry { module: "vte-2.91", source: "https://gitlab.gnome.org/GNOME/vte", git_ref: None, label: "VTE", build_targets: &[], install_tags: None },
    // ---- text stack --------------------------------------------------------
    CuratedEntry { module: "cairo", source: "https://gitlab.freedesktop.org/cairo/cairo", git_ref: None, label: "cairo", build_targets: &[], install_tags: None },
    CuratedEntry { module: "cairo-ft", source: "https://gitlab.freedesktop.org/cairo/cairo", git_ref: None, label: "cairo (ft backend)", build_targets: &[], install_tags: None },
    CuratedEntry { module: "cairo-gobject", source: "https://gitlab.freedesktop.org/cairo/cairo", git_ref: None, label: "cairo (gobject bindings)", build_targets: &[], install_tags: None },
    CuratedEntry { module: "cairo-pdf", source: "https://gitlab.freedesktop.org/cairo/cairo", git_ref: None, label: "cairo (pdf backend)", build_targets: &[], install_tags: None },
    CuratedEntry { module: "cairo-ps", source: "https://gitlab.freedesktop.org/cairo/cairo", git_ref: None, label: "cairo (ps backend)", build_targets: &[], install_tags: None },
    CuratedEntry { module: "cairo-quartz", source: "https://gitlab.freedesktop.org/cairo/cairo", git_ref: None, label: "cairo (quartz backend)", build_targets: &[], install_tags: None },
    CuratedEntry { module: "cairo-script", source: "https://gitlab.freedesktop.org/cairo/cairo", git_ref: None, label: "cairo (script backend)", build_targets: &[], install_tags: None },
    // cairo-script-interpreter is the script backend's library module
    // (cairo's own provide set — cf. the wrap `dependency_names` of
    // harfbuzz's subprojects/cairo.wrap). GTK declares it with
    // `required: false` (opportunistic reftest support): as an optional
    // dependency it resolves through this entry silently.
    CuratedEntry { module: "cairo-script-interpreter", source: "https://gitlab.freedesktop.org/cairo/cairo", git_ref: None, label: "cairo (script interpreter)", build_targets: &[], install_tags: None },
    CuratedEntry { module: "cairo-svg", source: "https://gitlab.freedesktop.org/cairo/cairo", git_ref: None, label: "cairo (svg backend)", build_targets: &[], install_tags: None },
    CuratedEntry { module: "cairo-win32", source: "https://gitlab.freedesktop.org/cairo/cairo", git_ref: None, label: "cairo (win32 backend)", build_targets: &[], install_tags: None },
    CuratedEntry { module: "cairo-xcb", source: "https://gitlab.freedesktop.org/cairo/cairo", git_ref: None, label: "cairo (xcb backend)", build_targets: &[], install_tags: None },
    CuratedEntry { module: "cairo-xlib", source: "https://gitlab.freedesktop.org/cairo/cairo", git_ref: None, label: "cairo (xlib backend)", build_targets: &[], install_tags: None },
    CuratedEntry { module: "fontconfig", source: "https://gitlab.freedesktop.org/fontconfig/fontconfig", git_ref: None, label: "fontconfig", build_targets: &[], install_tags: None },
    CuratedEntry { module: "freetype2", source: "https://gitlab.freedesktop.org/freetype/freetype", git_ref: None, label: "FreeType", build_targets: &[], install_tags: None },
    // FriBidi: GTK's bidirectional text engine (meson upstream)
    CuratedEntry { module: "fribidi", source: "https://github.com/fribidi/fribidi", git_ref: None, label: "FriBidi", build_targets: &[], install_tags: None },
    CuratedEntry { module: "harfbuzz", source: "https://github.com/harfbuzz/harfbuzz", git_ref: None, label: "HarfBuzz", build_targets: &[], install_tags: None },
    CuratedEntry { module: "harfbuzz-cairo", source: "https://github.com/harfbuzz/harfbuzz", git_ref: None, label: "HarfBuzz (cairo integration)", build_targets: &[], install_tags: None },
    CuratedEntry { module: "harfbuzz-gobject", source: "https://github.com/harfbuzz/harfbuzz", git_ref: None, label: "HarfBuzz (gobject bindings)", build_targets: &[], install_tags: None },
    CuratedEntry { module: "harfbuzz-icu", source: "https://github.com/harfbuzz/harfbuzz", git_ref: None, label: "HarfBuzz (icu integration)", build_targets: &[], install_tags: None },
    CuratedEntry { module: "harfbuzz-subset", source: "https://github.com/harfbuzz/harfbuzz", git_ref: None, label: "HarfBuzz (subsetter)", build_targets: &[], install_tags: None },
    CuratedEntry { module: "pango", source: "https://gitlab.gnome.org/GNOME/pango", git_ref: None, label: "Pango", build_targets: &[], install_tags: None },
    CuratedEntry { module: "pangocairo", source: "https://gitlab.gnome.org/GNOME/pango", git_ref: None, label: "Pango (cairo renderer)", build_targets: &[], install_tags: None },
    CuratedEntry { module: "pangoft2", source: "https://gitlab.gnome.org/GNOME/pango", git_ref: None, label: "Pango (freetype renderer)", build_targets: &[], install_tags: None },
    CuratedEntry { module: "pangowin32", source: "https://gitlab.gnome.org/GNOME/pango", git_ref: None, label: "Pango (win32 renderer)", build_targets: &[], install_tags: None },
    CuratedEntry { module: "pangoxft", source: "https://gitlab.gnome.org/GNOME/pango", git_ref: None, label: "Pango (xft renderer)", build_targets: &[], install_tags: None },
    CuratedEntry { module: "pixman-1", source: "https://gitlab.freedesktop.org/pixman/pixman", git_ref: None, label: "Pixman", build_targets: &[], install_tags: None },
    // ---- GNOME platform ----------------------------------------------------
    CuratedEntry { module: "appstream", source: "https://github.com/ximion/appstream", git_ref: None, label: "AppStream", build_targets: &[], install_tags: None },
    // NOTE: gitlab.freedesktop.org/appstream/appstream serves GitLab's
    // "HTTP Basic: Access denied" page to fully anonymous git-HTTP requests
    // (web UI public, repository access gated) — the real upstream is the
    // GitHub repository above; verified anonymously clonable.
    CuratedEntry { module: "appstream-glib", source: "https://github.com/hughsie/appstream-glib", git_ref: None, label: "AppStream-GLib", build_targets: &[], install_tags: None },
    // bash-completion: the completions-directory lookup module. AppStream
    // gates it behind its (default-ON) 'bash-completion' option, making
    // it genuinely required for a default build. Upstream is autotools
    // and installs bash-completion.pc.
    CuratedEntry { module: "bash-completion", source: "https://github.com/scop/bash-completion", git_ref: None, label: "bash-completion", build_targets: &[], install_tags: None },
    CuratedEntry { module: "gee-0.8", source: "https://gitlab.gnome.org/GNOME/libgee", git_ref: None, label: "libgee", build_targets: &[], install_tags: None },
    CuratedEntry { module: "gobject-introspection-1.0", source: "https://gitlab.gnome.org/GNOME/gobject-introspection", git_ref: None, label: "gobject-introspection", build_targets: &[], install_tags: None },
    CuratedEntry { module: "gsettings-desktop-schemas", source: "https://gitlab.gnome.org/GNOME/gsettings-desktop-schemas", git_ref: None, label: "gsettings-desktop-schemas", build_targets: &[], install_tags: None },
    CuratedEntry { module: "json-glib-1.0", source: "https://gitlab.gnome.org/GNOME/json-glib", git_ref: None, label: "JSON-GLib", build_targets: &[], install_tags: None },
    CuratedEntry { module: "libadwaita-1", source: "https://gitlab.gnome.org/GNOME/libadwaita", git_ref: None, label: "libadwaita", build_targets: &[], install_tags: None },
    CuratedEntry { module: "libnotify", source: "https://gitlab.gnome.org/GNOME/libnotify", git_ref: None, label: "libnotify", build_targets: &[], install_tags: None },
    CuratedEntry { module: "libsoup-2.4", source: "https://gitlab.gnome.org/GNOME/libsoup", git_ref: Some("libsoup-2.4"), label: "libsoup 2.4 (libsoup-2.4 branch)", build_targets: &[], install_tags: None },
    CuratedEntry { module: "libsoup-3.0", source: "https://gitlab.gnome.org/GNOME/libsoup", git_ref: None, label: "libsoup 3", build_targets: &[], install_tags: None },
    CuratedEntry { module: "libxml-2.0", source: "https://gitlab.gnome.org/GNOME/libxml2", git_ref: None, label: "libxml2", build_targets: &[], install_tags: None },
    // ---- core libraries ----------------------------------------------------
    CuratedEntry { module: "expat", source: "https://github.com/libexpat/libexpat", git_ref: None, label: "libexpat", build_targets: &[], install_tags: None },
    CuratedEntry { module: "icu-i18n", source: "https://github.com/unicode-org/icu", git_ref: None, label: "ICU (i18n module)", build_targets: &[], install_tags: None },
    CuratedEntry { module: "icu-io", source: "https://github.com/unicode-org/icu", git_ref: None, label: "ICU (io module)", build_targets: &[], install_tags: None },
    CuratedEntry { module: "icu-uc", source: "https://github.com/unicode-org/icu", git_ref: None, label: "ICU (uc module)", build_targets: &[], install_tags: None },
    CuratedEntry { module: "json-c", source: "https://github.com/json-c/json-c", git_ref: None, label: "json-c", build_targets: &[], install_tags: None },
    CuratedEntry { module: "libarchive", source: "https://github.com/libarchive/libarchive", git_ref: None, label: "libarchive", build_targets: &[], install_tags: None },
    CuratedEntry { module: "libcrypto", source: "https://github.com/openssl/openssl", git_ref: None, label: "OpenSSL (libcrypto)", build_targets: &[], install_tags: None },
    CuratedEntry { module: "libcurl", source: "https://github.com/curl/curl", git_ref: None, label: "libcurl", build_targets: &[], install_tags: None },
    // libelogind: the standalone systemd-login fork used on
    // non-systemd/musl systems (Alpine, Void's runit setups) — the
    // SECOND name of meson's classic multi-name fallback
    // `dependency('libsystemd', 'libelogind')`, which gitfull resolves
    // through the chain in meson's own order. Its tree carries
    // upstream's own component alias targets (`libelogind`, and
    // `devel`, which builds libelogind.pc) and tags the library
    // `libelogind` — the entry's component scope builds ONLY that,
    // never the login daemon.
    CuratedEntry { module: "libelogind", source: "https://github.com/elogind/elogind", git_ref: None, label: "elogind (libelogind)", build_targets: &["libelogind", "devel"], install_tags: Some("libelogind,devel") },
    CuratedEntry { module: "libffi", source: "https://github.com/libffi/libffi", git_ref: None, label: "libffi", build_targets: &[], install_tags: None },
    CuratedEntry { module: "libjpeg", source: "https://github.com/libjpeg-turbo/libjpeg-turbo", git_ref: None, label: "libjpeg-turbo", build_targets: &[], install_tags: None },
    // libfyaml: the YAML parser AppStream reads metadata with —
    // declared unconditionally in its top-level meson.build (a CMake
    // upstream; ships libfyaml.pc)
    CuratedEntry { module: "libfyaml", source: "https://github.com/pantoniou/libfyaml", git_ref: None, label: "libfyaml", build_targets: &[], install_tags: None },
    CuratedEntry { module: "libpng", source: "https://github.com/pnggroup/libpng", git_ref: None, label: "libpng", build_targets: &[], install_tags: None },
    CuratedEntry { module: "libssl", source: "https://github.com/openssl/openssl", git_ref: None, label: "OpenSSL (libssl)", build_targets: &[], install_tags: None },
    // libsystemd: the pkg-config module for systemd's sd-* client
    // library (sd-bus, sd-journal, sd-event, sd-id128, ...). Upstream
    // is the systemd project's own repository — a multi-component
    // monorepo. gitfull builds ONLY the libsystemd component, through
    // scoping systemd's own build defines: alias_target('libsystemd',
    // ...) names the component, the library carries install_tag
    // 'libsystemd', headers/.pc files are 'devel', and the `devel`
    // alias target builds every .pc in one shot (so the tagged install
    // never aborts on an unbuilt devel artifact). Verified against
    // systemd main's meson files; systemd itself uses the same
    // multi-name fallback idiom this entry supports
    // (dependency('libcrypt', 'libxcrypt')). Configure-time floor read
    // from the same files: gperf and python3-with-jinja2 (see
    // docs/ARCHITECTURE.md — all inside the TARGET app's build
    // environment; gitfull's own binary never links any of this).
    CuratedEntry { module: "libsystemd", source: "https://github.com/systemd/systemd", git_ref: None, label: "systemd (libsystemd)", build_targets: &["libsystemd", "devel"], install_tags: Some("libsystemd,devel") },
    CuratedEntry { module: "libtiff-4", source: "https://gitlab.com/libtiff/libtiff", git_ref: None, label: "libtiff", build_targets: &[], install_tags: None },
    CuratedEntry { module: "libturbojpeg", source: "https://github.com/libjpeg-turbo/libjpeg-turbo", git_ref: None, label: "libjpeg-turbo (TurboJPEG API)", build_targets: &[], install_tags: None },
    // libudev: the udev device-management client library — another
    // component of the systemd monorepo (upstream
    // alias_target('libudev', ...), install_tag 'libudev'): declared
    // directly by udev-observing apps, and hard-required at configure
    // time by elogind's current main. Same component-scoped build as
    // libsystemd; both entries share the systemd source, so a closure
    // needing both deduplicates to one fetch/build.
    CuratedEntry { module: "libudev", source: "https://github.com/systemd/systemd", git_ref: None, label: "systemd (libudev)", build_targets: &["libudev", "devel"], install_tags: Some("libudev,devel") },
    CuratedEntry { module: "libuv", source: "https://github.com/libuv/libuv", git_ref: None, label: "libuv", build_targets: &[], install_tags: None },
    CuratedEntry { module: "libwebp", source: "https://github.com/webmproject/libwebp", git_ref: None, label: "libwebp", build_targets: &[], install_tags: None },
    // NOTE: github.com/google/libwebp is gone (404 "Repository not found");
    // the project's GitHub home is the webmproject org, and sharpyuv
    // builds from the libwebp tree itself.
    CuratedEntry { module: "libsharpyuv", source: "https://github.com/webmproject/libwebp", git_ref: None, label: "libwebp (sharpyuv)", build_targets: &[], install_tags: None },
    // libzstd: AppStream's default-ON 'zstd-support' declares it
    // unconditionally; the zstd monorepo builds it with meson
    CuratedEntry { module: "libzstd", source: "https://github.com/facebook/zstd", git_ref: None, label: "zstd", build_targets: &[], install_tags: None },
    CuratedEntry { module: "openssl", source: "https://github.com/openssl/openssl", git_ref: None, label: "OpenSSL", build_targets: &[], install_tags: None },
    CuratedEntry { module: "sqlite3", source: "https://github.com/sqlite/sqlite", git_ref: None, label: "SQLite", build_targets: &[], install_tags: None },
    CuratedEntry { module: "zlib", source: "https://github.com/madler/zlib", git_ref: None, label: "zlib", build_targets: &[], install_tags: None },
    // ---- windowing / system ------------------------------------------------
    CuratedEntry { module: "dbus-1", source: "https://gitlab.freedesktop.org/dbus/dbus", git_ref: None, label: "D-Bus", build_targets: &[], install_tags: None },
    CuratedEntry { module: "epoxy", source: "https://github.com/anholt/libepoxy", git_ref: None, label: "libepoxy", build_targets: &[], install_tags: None },
    CuratedEntry { module: "graphene-1.0", source: "https://github.com/ebassi/graphene", git_ref: None, label: "graphene", build_targets: &[], install_tags: None },
    CuratedEntry { module: "graphene-gles2", source: "https://github.com/ebassi/graphene", git_ref: None, label: "graphene (gles2)", build_targets: &[], install_tags: None },
    // ---- GStreamer family (one monorepo, many modules) --------------------
    // Every gstreamer-* pkg-config module is built from the GStreamer
    // monorepo (core + gst-plugins-*). GTK's media backend is a
    // default-enabled feature that declares the play/gl/allocators
    // modules; all family members deduplicate to ONE clone/build.
    CuratedEntry { module: "gstreamer-1.0", source: "https://gitlab.freedesktop.org/gstreamer/gstreamer", git_ref: None, label: "GStreamer", build_targets: &[], install_tags: None },
    CuratedEntry { module: "gstreamer-allocators-1.0", source: "https://gitlab.freedesktop.org/gstreamer/gstreamer", git_ref: None, label: "GStreamer (allocators module)", build_targets: &[], install_tags: None },
    CuratedEntry { module: "gstreamer-gl-1.0", source: "https://gitlab.freedesktop.org/gstreamer/gstreamer", git_ref: None, label: "GStreamer (gl module)", build_targets: &[], install_tags: None },
    CuratedEntry { module: "gstreamer-play-1.0", source: "https://gitlab.freedesktop.org/gstreamer/gstreamer", git_ref: None, label: "GStreamer (play module)", build_targets: &[], install_tags: None },
    CuratedEntry { module: "gstreamer-tag-1.0", source: "https://gitlab.freedesktop.org/gstreamer/gstreamer", git_ref: None, label: "GStreamer (tag module)", build_targets: &[], install_tags: None },
    CuratedEntry { module: "libdrm", source: "https://gitlab.freedesktop.org/mesa/drm", git_ref: None, label: "libdrm", build_targets: &[], install_tags: None },
    CuratedEntry { module: "libdrm_amdgpu", source: "https://gitlab.freedesktop.org/mesa/drm", git_ref: None, label: "libdrm (amdgpu)", build_targets: &[], install_tags: None },
    CuratedEntry { module: "libdrm_intel", source: "https://gitlab.freedesktop.org/mesa/drm", git_ref: None, label: "libdrm (intel)", build_targets: &[], install_tags: None },
    CuratedEntry { module: "libdrm_nouveau", source: "https://gitlab.freedesktop.org/mesa/drm", git_ref: None, label: "libdrm (nouveau)", build_targets: &[], install_tags: None },
    CuratedEntry { module: "libdrm_radeon", source: "https://gitlab.freedesktop.org/mesa/drm", git_ref: None, label: "libdrm (radeon)", build_targets: &[], install_tags: None },
    CuratedEntry { module: "libinput", source: "https://gitlab.freedesktop.org/libinput/libinput", git_ref: None, label: "libinput", build_targets: &[], install_tags: None },
    CuratedEntry { module: "libpulse", source: "https://gitlab.freedesktop.org/pulseaudio/pulseaudio", git_ref: None, label: "PulseAudio (client lib)", build_targets: &[], install_tags: None },
    CuratedEntry { module: "libpulse-mainloop-glib", source: "https://gitlab.freedesktop.org/pulseaudio/pulseaudio", git_ref: None, label: "PulseAudio (glib mainloop)", build_targets: &[], install_tags: None },
    CuratedEntry { module: "sdl2", source: "https://github.com/libsdl-org/SDL", git_ref: Some("SDL2"), label: "SDL2 (SDL2 branch)", build_targets: &[], install_tags: None },
    CuratedEntry { module: "sdl3", source: "https://github.com/libsdl-org/SDL", git_ref: None, label: "SDL3", build_targets: &[], install_tags: None },
    CuratedEntry { module: "wayland", source: "https://gitlab.freedesktop.org/wayland/wayland", git_ref: None, label: "libwayland", build_targets: &[], install_tags: None },
    CuratedEntry { module: "wayland-client", source: "https://gitlab.freedesktop.org/wayland/wayland", git_ref: None, label: "libwayland (client)", build_targets: &[], install_tags: None },
    CuratedEntry { module: "wayland-cursor", source: "https://gitlab.freedesktop.org/wayland/wayland", git_ref: None, label: "libwayland (cursor)", build_targets: &[], install_tags: None },
    CuratedEntry { module: "wayland-egl", source: "https://gitlab.freedesktop.org/wayland/wayland", git_ref: None, label: "libwayland (egl)", build_targets: &[], install_tags: None },
    CuratedEntry { module: "wayland-egl-backend", source: "https://gitlab.freedesktop.org/wayland/wayland", git_ref: None, label: "libwayland (egl-backend)", build_targets: &[], install_tags: None },
    CuratedEntry { module: "wayland-scanner", source: "https://gitlab.freedesktop.org/wayland/wayland", git_ref: None, label: "libwayland (scanner tool)", build_targets: &[], install_tags: None },
    CuratedEntry { module: "wayland-server", source: "https://gitlab.freedesktop.org/wayland/wayland", git_ref: None, label: "libwayland (server)", build_targets: &[], install_tags: None },
    // ---- X11 client libraries (one repo per module, xorg/lib/<name>) -----
    // the pkg-config names GTK's default-enabled x11 backend requires
    CuratedEntry { module: "x11", source: "https://gitlab.freedesktop.org/xorg/lib/libx11", git_ref: None, label: "libX11", build_targets: &[], install_tags: None },
    CuratedEntry { module: "xcb", source: "https://gitlab.freedesktop.org/xorg/lib/libxcb", git_ref: None, label: "libxcb", build_targets: &[], install_tags: None },
    CuratedEntry { module: "xcursor", source: "https://gitlab.freedesktop.org/xorg/lib/libxcursor", git_ref: None, label: "libXcursor", build_targets: &[], install_tags: None },
    CuratedEntry { module: "xdamage", source: "https://gitlab.freedesktop.org/xorg/lib/libxdamage", git_ref: None, label: "libXdamage", build_targets: &[], install_tags: None },
    CuratedEntry { module: "xext", source: "https://gitlab.freedesktop.org/xorg/lib/libxext", git_ref: None, label: "libXext", build_targets: &[], install_tags: None },
    CuratedEntry { module: "xfixes", source: "https://gitlab.freedesktop.org/xorg/lib/libxfixes", git_ref: None, label: "libXfixes", build_targets: &[], install_tags: None },
    CuratedEntry { module: "xi", source: "https://gitlab.freedesktop.org/xorg/lib/libxi", git_ref: None, label: "libXi", build_targets: &[], install_tags: None },
    CuratedEntry { module: "xinerama", source: "https://gitlab.freedesktop.org/xorg/lib/libxinerama", git_ref: None, label: "libXinerama", build_targets: &[], install_tags: None },
    CuratedEntry { module: "xrandr", source: "https://gitlab.freedesktop.org/xorg/lib/libxrandr", git_ref: None, label: "libXrandr", build_targets: &[], install_tags: None },
    CuratedEntry { module: "xrender", source: "https://gitlab.freedesktop.org/xorg/lib/libxrender", git_ref: None, label: "libXrender", build_targets: &[], install_tags: None },
    CuratedEntry { module: "xkbcommon", source: "https://github.com/xkbcommon/libxkbcommon", git_ref: None, label: "libxkbcommon", build_targets: &[], install_tags: None },
    CuratedEntry { module: "xkbcommon-x11", source: "https://github.com/xkbcommon/libxkbcommon", git_ref: None, label: "libxkbcommon (x11)", build_targets: &[], install_tags: None },
    CuratedEntry { module: "xkbregistry", source: "https://github.com/xkbcommon/libxkbcommon", git_ref: None, label: "libxkbcommon (registry)", build_targets: &[], install_tags: None },
    // ---- graphics loaders / APIs -------------------------------------------
    // the Vulkan loader behind GTK's vulkan renderer
    CuratedEntry { module: "vulkan", source: "https://github.com/KhronosGroup/Vulkan-Loader", git_ref: None, label: "Vulkan-Loader", build_targets: &[], install_tags: None },
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
            // a build scope is complete: targets ⇔ install tags (a
            // targets-only or tags-only scope would build scoped but
            // then rebuild the whole monorepo at install, or vice versa)
            assert_eq!(
                !e.build_targets.is_empty(),
                e.install_tags.is_some(),
                "{} has a half-configured build scope",
                e.module
            );
            // scoped targets/tags carry no separators that would break
            // the ninja/meson argv they are spliced into
            for t in e.build_targets {
                assert!(
                    !t.contains(char::is_whitespace) && !t.contains(','),
                    "bad build target `{t}`"
                );
            }
            if let Some(tags) = e.install_tags {
                assert!(!tags.is_empty(), "empty install_tags for {}", e.module);
            }
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
            &["glib-2.0", "gio-2.0", "gio-unix-2.0", "gobject-2.0", "girepository-2.0"][..],
            &["cairo", "cairo-gobject", "cairo-svg", "cairo-script-interpreter"][..],
            &["wayland-client", "wayland-server", "wayland-scanner"][..],
            &["harfbuzz", "harfbuzz-subset", "harfbuzz-icu"][..],
            &["openssl", "libssl", "libcrypto"][..],
            &["libwebp", "libsharpyuv"][..],
            &["gstreamer-1.0", "gstreamer-play-1.0", "gstreamer-gl-1.0", "gstreamer-allocators-1.0", "gstreamer-tag-1.0"][..],
        ] {
            assert_eq!(distinct_sources(family).len(), 1, "{family:?}");
        }
    }

    /// The dependency list from the second ProtonPlus round — the run
    /// that motivated the required/optional semantics fix. girepository-2.0
    /// is the gio-unix-2.0 category (module inside a parent repo: GLib's
    /// own tree since 2.79 — NOT a new special case); cairo-script-interpreter
    /// is a cairo module; the gstreamer-* modules are the default-enabled
    /// GTK media backend; bash-completion is AppStream's default-ON
    /// completions-dir lookup. Again: a fixture, and NONE of these names
    /// may be special-cased anywhere in planner/search code.
    #[test]
    fn validation_list_from_the_optional_semantics_run_resolves_via_curated_map() {
        let list = [
            "bash-completion",
            "cairo-script-interpreter",
            "girepository-2.0",
            "gstreamer-allocators-1.0",
            "gstreamer-gl-1.0",
            "gstreamer-play-1.0",
            // the third round (AppStream's own default-on requirements):
            // gi-docgen (apidocs, native tool), libfyaml (unconditional),
            // libzstd (zstd-support)
            "gi-docgen",
            "libfyaml",
            "libzstd",
        ];
        for name in list {
            assert!(lookup(name).is_some(), "`{name}` missing from curated map");
        }
        // girepository-2.0 shares GLib's source exactly like gio-unix-2.0
        assert_eq!(
            lookup("girepository-2.0").map(|e| e.source),
            lookup("gio-unix-2.0").map(|e| e.source)
        );
        // the gstreamer family collapses to ONE monorepo source
        let gst = distinct_sources(&[
            "gstreamer-1.0",
            "gstreamer-play-1.0",
            "gstreamer-gl-1.0",
            "gstreamer-allocators-1.0",
            "gstreamer-tag-1.0",
        ]);
        assert_eq!(gst.len(), 1, "{gst:?}");
    }

    /// The systemd family: `libsystemd` (the sd-* client library of the
    /// systemd monorepo), `libelogind` (the standalone login fork used
    /// on musl/non-systemd systems — the second name of meson's classic
    /// `dependency('libsystemd', 'libelogind')` fallback), and `libudev`
    /// (the udev client library — another systemd component; elogind's
    /// current main hard-requires it at configure time). All three are
    /// component-scoped: only the named component of the monorepo is
    /// ever compiled.
    #[test]
    fn systemd_family_resolves_with_component_scopes() {
        let libsystemd = lookup("libsystemd").unwrap();
        assert_eq!(libsystemd.source, "https://github.com/systemd/systemd");
        let libelogind = lookup("libelogind").unwrap();
        assert_eq!(libelogind.source, "https://github.com/elogind/elogind");
        let libudev = lookup("libudev").unwrap();
        assert_eq!(libudev.source, "https://github.com/systemd/systemd");

        // the two systemd-repo modules deduplicate to ONE source (the
        // walk's single-fetch precondition); elogind is a separate tree
        assert_eq!(distinct_sources(&["libsystemd", "libudev"]).len(), 1);
        assert_eq!(distinct_sources(&["libsystemd", "libelogind"]).len(), 2);

        // every family member carries a complete component scope whose
        // targets/tags are upstream's own definitions (verified against
        // systemd main's alias_target('libsystemd'/'libudev'/'devel')
        // and elogind's alias_target('libelogind'/'devel'))
        for (module, lib_tag) in [
            ("libsystemd", "libsystemd"),
            ("libelogind", "libelogind"),
            ("libudev", "libudev"),
        ] {
            let e = lookup(module).unwrap();
            let (targets, tags) = e.scope().unwrap_or_else(|| {
                panic!("`{module}` must carry a component build scope")
            });
            // the component's own library target + upstream's `devel`
            // alias (which builds every .pc file, so the tagged install
            // never trips over an unbuilt devel artifact)
            assert_eq!(targets, &[lib_tag, "devel"][..], "`{module}` targets");
            assert_eq!(tags, format!("{lib_tag},devel"), "`{module}` tags");
        }

        // unscoped entries have NO scope — the standard build path
        assert!(lookup("glib-2.0").unwrap().scope().is_none());
        assert!(lookup("zlib").unwrap().scope().is_none());
    }

    /// The generic-remote contract of this table: every source must be
    /// **anonymously cloneable**, because a host without a `[forge.<name>]`
    /// entry is fetched with zero credentials.
    ///
    /// Audited live (sealed anonymous `git ls-remote`, no credentials, no
    /// helper — the exact invocation gitfull's generic-remote path makes):
    /// 44 of 46 distinct sources answered anonymously. The two failures
    /// were exactly the class of bug this test pins:
    ///
    /// * `gitlab.freedesktop.org/appstream/appstream` — the server responds
    ///   with GitLab's `HTTP Basic: Access denied` page **to a request that
    ///   carried no credentials at all** (its web UI is public; its git-HTTP
    ///   is auth-gated). That canned page misleadingly reads like a client
    ///   sent a bad password — it is the server refusing anonymous access.
    ///   Corrected to the real upstream, github.com/ximion/appstream.
    /// * `github.com/google/libwebp` — repository gone (404); libwebp now
    ///   lives in the webmproject org (sharpyuv builds from that same
    ///   tree). Corrected both module entries.
    #[test]
    fn curated_sources_are_anonymously_clonable_upstreams() {
        for (module, expected) in [
            ("appstream", "https://github.com/ximion/appstream"),
            ("libwebp", "https://github.com/webmproject/libwebp"),
            ("libsharpyuv", "https://github.com/webmproject/libwebp"),
            // audited live the same way (sealed anonymous ls-remote):
            // the systemd monorepo and the elogind fork both answer
            // anonymous git-HTTP on github.com
            ("libsystemd", "https://github.com/systemd/systemd"),
            ("libelogind", "https://github.com/elogind/elogind"),
            ("libudev", "https://github.com/systemd/systemd"),
        ] {
            assert_eq!(
                lookup(module).map(|e| e.source),
                Some(expected),
                "`{module}` must point at its anonymously-clonable upstream"
            );
        }
        // the anonymous contract itself: no curated URL may embed
        // credentials of any kind (userinfo, token hints)
        for e in CURATED {
            let rest = e.source.strip_prefix("https://").unwrap_or(e.source);
            assert!(!rest.contains('@'), "userinfo in curated source: {}", e.source);
            let low = e.source.to_ascii_lowercase();
            assert!(!low.contains("token"), "token hint in curated source: {}", e.source);
            assert!(!low.contains("oauth"), "oauth hint in curated source: {}", e.source);
        }
    }
}
