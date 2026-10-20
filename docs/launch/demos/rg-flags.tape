Require wg
Require rg
Output docs/launch/renders/raw/wg-rg-flags-16x9.mp4
Set Shell "bash"
Set Width 1920
Set Height 620
Set FontSize 43
Set FontFamily "DejaVu Sans Mono"
Set Theme "Catppuccin Mocha"
Set Padding 16
Set Margin 10
Set MarginFill "#181825"
Set BorderRadius 20
Set WindowBar Colorful
Set WindowBarSize 48
Set TypingSpeed 8ms
Set Framerate 30
Set CursorBlink false
Set PlaybackSpeed 1
Set WaitTimeout 3m
Hide
Type "source docs/launch/demos/prepare.sh"
Enter
Sleep 1s
Type "wg up --model code"
Enter
Wait+Screen /PID:/
Wait+Line /\$$/
Sleep 1s
Type "clear"
Enter
Sleep 500ms
Show
Type `wg -n -F "struct Flags" .`
Enter
Sleep 2000ms
Type `wg -l "fn normalize_args" .`
Enter
Sleep 2000ms
Type `wg -g "*.rs" -n -F "struct Flags" .`
Enter
Sleep 3000ms
Hide
Type "wg down"
Enter
Sleep 1500ms
