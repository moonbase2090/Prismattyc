#!/bin/bash
# Smooth 24-bit truecolor rainbow, drawn cell by cell.
echo "24-bit truecolor gradient:"
awk 'BEGIN{
  cols=88;
  for(row=0;row<4;row++){
    for(i=0;i<cols;i++){
      t=i/cols*6.2831853;
      r=int(127+127*sin(t+0.0));
      g=int(127+127*sin(t+2.0943951));
      b=int(127+127*sin(t+4.1887902));
      printf "\033[48;2;%d;%d;%dm ", r, g, b;
    }
    printf "\033[0m\n";
  }
}'
echo "256-color ramp:"
for i in $(seq 232 255); do printf "\033[48;5;%dm \033[0m" "$i"; done
echo
