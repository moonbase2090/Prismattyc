#!/bin/bash
# Color card shown during the footer walk and theme demo.
# - 16 ANSI swatches at the top so a theme change repaints live.
# The truecolor gradient lives in colors.sh (shown in its own beat).
names=(Black Red Green Yellow Blue Magenta Cyan White)
printf '\n  ANSI 0–15 (theme-mapped)\n'
printf '  normal  '
for i in 0 1 2 3 4 5 6 7; do printf '\033[3%dm%-8s\033[0m' "$i" "${names[$i]}"; done
printf '\n  bright  '
for i in 0 1 2 3 4 5 6 7; do printf '\033[9%dm%-8s\033[0m' "$i" "${names[$i]}"; done
printf '\n  '
for i in 0 1 2 3 4 5 6 7; do printf '\033[4%dm      \033[0m ' "$i"; done
printf '\n  '
for i in 0 1 2 3 4 5 6 7; do printf '\033[10%dm      \033[0m ' "$i"; done
printf '\n'
