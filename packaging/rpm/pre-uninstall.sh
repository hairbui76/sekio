# Undo what post-install.sh enabled.
#
# This is %preun and not %postun for the reason packaging/deb/prerm spells out:
# once /usr/lib/systemd/user/sekio.service has been deleted,
# `systemctl --global disable sekio.service` fails with "unit sekio.service
# does not exist" and leaves the symlink under /etc/systemd/user dangling.
# %preun still has the unit file on disk.
#
# `$1` is the number of versions that will remain: 0 on a real uninstall, 1 on
# an upgrade (where the old package's %preun also runs, and must not disable
# anything -- post-install.sh only enables on a first install, so a disable
# here would leave every upgrade with the daemon switched off).
set -e

if [ "$1" = 0 ]; then
    if command -v systemctl >/dev/null 2>&1; then
        systemctl --global disable sekio.service || :
    fi
    # Belt and braces, as in the .deb: never leave a symlink in /etc pointing
    # at a unit file that has been removed.
    rm -f /etc/systemd/user/graphical-session.target.wants/sekio.service
    rmdir --ignore-fail-on-non-empty \
        /etc/systemd/user/graphical-session.target.wants 2>/dev/null || :
fi

exit 0
