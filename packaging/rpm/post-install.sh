# Enable the sekio preview daemon's systemd *user* unit for every user.
# The .deb twin of this script is packaging/deb/postinst, which carries the
# full reasoning; the short version:
#
#   `systemctl --user enable` is useless here -- an rpm scriptlet runs as root,
#   which has no graphical session, and cannot reach into the home directory of
#   every present and future user. `systemctl --global enable` writes only
#   /etc/systemd/user/graphical-session.target.wants/sekio.service, needs no
#   running user manager, and covers all future logins of all users.
#   `systemctl --global disable sekio` is the exact inverse, and is the off
#   switch README.md documents.
#
# `$1` is the number of installed versions of this package: 1 on a first
# install, 2 or more on an upgrade. Enabling on first install only is what
# makes an administrator's `--global disable` survive later upgrades.
#
# --global affects future logins, so an already-open session is untouched;
# `systemctl --user start sekio` starts it there and then.
set -e

if [ "$1" = 1 ] && command -v systemctl >/dev/null 2>&1; then
    if ! systemctl --global enable sekio.service; then
        echo "sekio: could not enable the preview daemon automatically." >&2
        echo "sekio: turn it on with 'systemctl --user enable --now sekio'." >&2
    fi
fi

exit 0
