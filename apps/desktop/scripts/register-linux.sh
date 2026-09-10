#!/bin/sh
set -eu
APP_PATH=${1:?usage: register-linux.sh /opt/agent-send/agent-send}
AUTOSTART="$HOME/.config/autostart/agent-send.desktop"
mkdir -p "$(dirname "$AUTOSTART")"
cat > "$AUTOSTART" <<EOF
[Desktop Entry]
Type=Application
Name=agent-send
Comment=Private local file transfer
Exec=$APP_PATH --hidden
Terminal=false
X-GNOME-Autostart-enabled=true
EOF
echo "Registered XDG autostart: $AUTOSTART"
