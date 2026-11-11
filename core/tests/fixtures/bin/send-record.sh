#!/bin/sh
# Records stdin to $1 and the environment to $1.env, then confirms the send.
cat > "$1"
env > "$1.env"
echo '{"status":"sent","provider_message_id":"p-1"}'
