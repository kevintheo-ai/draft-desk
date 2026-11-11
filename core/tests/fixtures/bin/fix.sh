#!/bin/sh
input=$(cat)
case "$input" in
  *"\"findings\":[{"*"\"title\":\"subject Re: Offer\""*) echo '{"body":"Fixed body for: subject Re: Offer"}' ;;
  *) echo '{"body":"no findings seen"}' ;;
esac
