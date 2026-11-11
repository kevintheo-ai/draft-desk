#!/bin/sh
n=$(wc -c | tr -d " ")
printf '{"verdict":"pass","findings":[{"level":"info","title":"%s"}]}\n' "$n"
