#!/bin/sh
# Echoes the item view (one line of JSON) back as the detail of a finding; the title is the item id.
escaped=$(sed -e 's/\\/\\\\/g' -e 's/"/\\"/g')
printf '{"verdict":"pass","findings":[{"level":"info","title":"%s","detail":"%s"}]}\n' "$DRAFTDESK_ITEM_ID" "$escaped"
