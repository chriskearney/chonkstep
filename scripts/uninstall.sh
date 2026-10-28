#!/usr/bin/env bash
# Undoes scripts/install.sh: removes chonkstep's login sessions, SDDM
# integration, portal declaration and the user-facing links, and leaves
# the machine logging in to Omarchy exactly as it did before.
#
# What this removes (only files install.sh, install-m3gpu-session.sh or
# dock/install.sh create - nothing is matched by wildcard):
#   - session entries: /usr/share/xsessions/chonkstep.desktop,
#     /usr/share/wayland-sessions/chonkstep{,-uwsm,-m3gpu-uwsm}.desktop
#   - SDDM: the chonkstep picker theme, the zz-chonkstep-{theme,autologin}
#     drop-ins (and the legacy 20-chonkstep-theme.conf), and the
#     sddm.service resilience drop-in, followed by a daemon-reload
#   - portals: chonkstep.portal and chonkstep-portals.conf
#   - ~/.local/bin links into a chonkstep checkout (chonkrec,
#     chonkstep-bugreport, omarchy-export-themes, chonk-netjoin,
#     chonk-dock, chonk-get, chonk-btpair), plus the Chonk Dock copies
#     dock/install.sh puts there, its docs and its .desktop entry
#   - Omarchy plugin symlinks (~/.config/omarchy/plugins/chonkstep.*)
#     that point into a chonkstep checkout
#
# What it leaves alone, deliberately:
#   - pacman dependencies (Xorg, foot, mesa, the portal stack, uwsm, ...);
#     other desktops share them, and removing them is your call
#   - this checkout and its target/ (run `cargo clean`, or delete it)
#   - themes exported into ~/.config/omarchy/themes; Omarchy may be
#     wearing one right now, so switch theme first and delete by hand
#   - your config and state, unless --purge is given:
#     ~/.config/chonkstep and ~/.local/state/chonkstep
#   - chonk-agents adapter hooks; `chonk-agents uninstall` owns those
#   - SDDM's own state (/var/lib/sddm): a remembered chonkstep session
#     that no longer exists simply falls back to the default entry
#
# An Arch package install is refused: pacman owns those files, so use
# `omarchy remove desktop-chonkstep` or `sudo pacman -R chonkstep`.
#
# Every step is safe to repeat; absent files are skipped, and sudo is
# only invoked when there is a system file to remove.
#
# Usage: scripts/uninstall.sh [--dry-run] [--purge] [--root DIR]
#   --dry-run   print what would be removed, change nothing
#   --purge     also delete ~/.config/chonkstep and ~/.local/state/chonkstep
#   --root DIR  operate on system paths beneath DIR (a staged tree, for
#               tests); no sudo, no daemon-reload, no package check
set -euo pipefail

usage() {
    echo "Usage: scripts/uninstall.sh [--dry-run] [--purge] [--root DIR]" >&2
    exit 2
}

dry_run=""
purge=""
root=""
while [ "$#" -gt 0 ]; do
    case "$1" in
        --dry-run) dry_run="yes" ;;
        --purge) purge="yes" ;;
        --root)
            [ "$#" -ge 2 ] || usage
            root="${2%/}"
            if [ -z "$root" ] || [ ! -d "$root" ]; then
                echo "--root must name an existing directory other than /" >&2
                exit 2
            fi
            shift
            ;;
        -h | --help) usage ;;
        *) usage ;;
    esac
    shift
done

removed=0

say() {
    if [ -n "$dry_run" ]; then
        echo "would remove: $1"
    else
        echo "removed: $1"
    fi
    removed=$((removed + 1))
}

exists() {
    [ -e "$1" ] || [ -L "$1" ]
}

# -- System scope -----------------------------------------------------

system_files=(
    /usr/share/xsessions/chonkstep.desktop
    /usr/share/wayland-sessions/chonkstep.desktop
    /usr/share/wayland-sessions/chonkstep-uwsm.desktop
    /usr/share/wayland-sessions/chonkstep-m3gpu-uwsm.desktop
    /usr/share/sddm/themes/chonkstep/Main.qml
    /usr/share/sddm/themes/chonkstep/metadata.desktop
    /usr/share/sddm/themes/chonkstep/theme.conf
    /etc/sddm.conf.d/zz-chonkstep-theme.conf
    /etc/sddm.conf.d/zz-chonkstep-autologin.conf
    /etc/sddm.conf.d/20-chonkstep-theme.conf
    /etc/systemd/system/sddm.service.d/90-chonkstep-resilience.conf
    /usr/share/xdg-desktop-portal/portals/chonkstep.portal
    /usr/share/xdg-desktop-portal/chonkstep-portals.conf
)
# Emptied by the removals above; removed only if nothing else is inside.
system_dirs=(
    /usr/share/sddm/themes/chonkstep
    /etc/systemd/system/sddm.service.d
)

# A package install owns these same paths. Deleting them behind pacman's
# back leaves a package that claims files it no longer has, so hand the
# job to the tools that know about it.
if [ -z "$root" ] && command -v pacman >/dev/null 2>&1; then
    for path in "${system_files[@]}"; do
        exists "$path" || continue
        if owner="$(pacman -Qoq "$path" 2>/dev/null)"; then
            echo "$path belongs to the '$owner' package; this script only undoes" >&2
            echo "scripts/install.sh. Remove the package instead:" >&2
            echo "    omarchy remove desktop-chonkstep    # restores Omarchy's login setup" >&2
            echo "    sudo pacman -R $owner                # without Omarchy" >&2
            exit 1
        fi
    done
fi

present=()
for path in "${system_files[@]}"; do
    if exists "$root$path"; then
        present+=("$root$path")
    fi
done

if [ "${#present[@]}" -gt 0 ]; then
    if [ -z "$dry_run" ]; then
        if [ -n "$root" ]; then
            rm -f -- "${present[@]}"
        else
            echo "Removing system integration (sudo)..."
            sudo rm -f -- "${present[@]}"
        fi
    fi
    for path in "${present[@]}"; do say "$path"; done
fi

for dir in "${system_dirs[@]}"; do
    dir="$root$dir"
    [ -d "$dir" ] || continue
    # A dry run removed nothing, so judge emptiness by what would remain.
    remaining="$(find "$dir" -mindepth 1 -maxdepth 1 | while IFS= read -r entry; do
        keep="yes"
        for path in "${present[@]+"${present[@]}"}"; do
            [ "$entry" = "$path" ] && keep=""
        done
        [ -z "$keep" ] || echo "$entry"
    done)"
    [ -z "$remaining" ] || continue
    if [ -z "$dry_run" ]; then
        if [ -n "$root" ]; then rmdir -- "$dir"; else sudo rmdir -- "$dir"; fi
    fi
    say "$dir/"
done

if [ "${#present[@]}" -gt 0 ] && [ -z "$root" ] && [ -z "$dry_run" ] &&
    [ -d /run/systemd/system ]; then
    sudo systemctl daemon-reload
fi

# -- User scope -------------------------------------------------------

repo="$(cd "$(dirname "$0")/.." && pwd)"
bin="$HOME/.local/bin"
data="${XDG_DATA_HOME:-$HOME/.local/share}"
config_home="${XDG_CONFIG_HOME:-$HOME/.config}"
state_home="${XDG_STATE_HOME:-$HOME/.local/state}"

remove_user() {
    [ -n "$dry_run" ] || rm -rf -- "$1"
    say "$1"
}

# name:checkout-relative target of the link install.sh makes. A link is
# removed when it points at that target in ANY checkout, so a moved or
# deleted clone's dangling links go too; a link pointing elsewhere is
# somebody else's and stays.
links=(
    chonkrec:scripts/chonkrec
    chonkstep-bugreport:scripts/chonkstep-bugreport
    omarchy-export-themes:target/release/omarchy-export-themes
    chonk-netjoin:target/release/chonk-netjoin
    chonk-dock:target/release/chonk-dock
    chonk-get:dock/chonk-get
    chonk-btpair:target/release/chonk-btpair
)
for spec in "${links[@]}"; do
    name="${spec%%:*}"
    suffix="${spec#*:}"
    path="$bin/$name"
    if [ -L "$path" ]; then
        case "$(readlink -- "$path")" in
            */"$suffix") remove_user "$path" ;;
            *) echo "kept: $path (links somewhere other than a chonkstep checkout)" ;;
        esac
    elif [ -f "$path" ]; then
        # dock/install.sh copies these four instead of linking them.
        case "$name" in
            chonk-dock | chonk-netjoin | chonk-btpair | chonk-get) remove_user "$path" ;;
            *) echo "kept: $path (a regular file, not install.sh's link)" ;;
        esac
    fi
done

desktop="$data/applications/chonk-dock.desktop"
if [ -f "$desktop" ]; then
    if cmp -s "$desktop" "$repo/dock/chonk-dock.desktop" ||
        grep -q '^Exec=chonk-dock\b' "$desktop"; then
        remove_user "$desktop"
    else
        echo "kept: $desktop (edited; does not launch chonk-dock)"
    fi
fi
[ ! -d "$data/doc/chonkstep" ] || remove_user "$data/doc/chonkstep"

for plugin in "$config_home"/omarchy/plugins/chonkstep.*; do
    exists "$plugin" || continue
    if [ -L "$plugin" ]; then
        case "$(readlink -- "$plugin")" in
            */omarchy/plugins/chonkstep.*) remove_user "$plugin" ;;
            *) echo "kept: $plugin (links outside a chonkstep checkout)" ;;
        esac
    else
        echo "kept: $plugin (a copied plugin, not a link; delete it by hand)"
    fi
done

if [ -n "$purge" ]; then
    [ ! -d "$config_home/chonkstep" ] || remove_user "$config_home/chonkstep"
    [ ! -d "$state_home/chonkstep" ] || remove_user "$state_home/chonkstep"
fi

# -- Summary ----------------------------------------------------------

echo
if [ "$removed" -eq 0 ]; then
    echo "Nothing to remove: chonkstep's install.sh integration is not present."
elif [ -n "$dry_run" ]; then
    echo "Dry run: $removed item(s) would be removed. Re-run without --dry-run to apply."
else
    echo "chonkstep is uninstalled ($removed item(s) removed)."
fi

if [ "${XDG_CURRENT_DESKTOP:-}" = "chonkstep" ]; then
    echo "  - This session is chonkstep and keeps running until you log out;"
    echo "    the next login uses Omarchy's normal session."
fi
current_theme="$config_home/omarchy/current/theme"
if [ -f "$current_theme/chonkstep.toml" ]; then
    echo "  - Omarchy is wearing an exported chonkstep theme. Pick another with"
    echo "    omarchy-theme-set before deleting it from $config_home/omarchy/themes."
elif [ -d "$config_home/omarchy/themes" ]; then
    echo "  - Exported chonkstep themes in $config_home/omarchy/themes were kept."
fi
if [ -z "$purge" ] &&
    { [ -d "$config_home/chonkstep" ] || [ -d "$state_home/chonkstep" ]; }; then
    echo "  - Kept $config_home/chonkstep and $state_home/chonkstep;"
    echo "    re-run with --purge to delete them."
fi
cat <<DONE
  - Packages installed as dependencies were left in place; other
    desktops use them too.
  - The checkout (${repo}) and its build output remain; delete it, or
    run \`cargo clean\` there, to reclaim the space.
DONE
