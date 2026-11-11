#!/bin/sh
cat >/dev/null
printf '%s\n' '{"verdict":"warn","findings":[{"level":"warn","title":"fine‮evil","detail":"a\u0007b\nnext","quote":"x​y","confirm":true}]}'
