#!/bin/bash
# Cell-grid sprites: box drawing, arcs, dashes, blocks, braille, powerline.
echo "Box:      ┌─────┬─────┐  ╔═════╦═════╗  ╭─────┬─────╮"
echo "          │ ─ │ │ ━ │ │  ║  ═  ║  ║  ║  │ ┄┄┄ │ ┅┅┅ │"
echo "          ├─────┼─────┤  ╠═════╬═════╣  ├─────┼─────┤"
echo "          │ ┆ ┇ │ ┊ ┋ │  ║  ╫  ║  ╪  ║  │ ╌╌╌ │ ╍╍╍ │"
echo "          └─────┴─────┘  ╚═════╩═════╝  ╰─────┴─────╯"
echo "Blocks:   ▏▎▍▌▋▊▉█  ▁▂▃▄▅▆▇█  ░▒▓  ▖▗▘▙▚▛▜▝▞▟"
echo "Braille:  ⠁⠃⠇⡇⣇⣧⣷⣿  ⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏  ⣾⣽⣻⢿⡿⣟⣯⣷"
echo "Sextants: 🬀🬁🬂🬃🬄🬅🬆🬇🬈🬉🬊🬋🬌🬍🬎🬏🬐🬑🬒🬓🬔🬕🬖🬗"
printf 'Powerline: \e[48;5;24m\e[97m main \e[0m\e[38;5;24m\e[48;5;31m\e[0m\e[48;5;31m\e[97m ✓ \e[0m\e[38;5;31m\e[0m  \e[38;5;208m\e[48;5;208m\e[30m demo@prismattyc \e[0m\n'
