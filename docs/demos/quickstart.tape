Require wg
Output docs/media/wg-quickstart.mp4
Set Shell "bash"
Set Width 1200
Set Height 314
Set FontSize 18
Set FontFamily "DejaVu Sans Mono"
Set Theme "Catppuccin Mocha"
Set Padding 8
Set Margin 6
Set MarginFill "#181825"
Set BorderRadius 10
Set TypingSpeed 8ms
Set Framerate 30
Set CursorBlink false
Set PlaybackSpeed 1
Set WaitTimeout 3m
Hide
Type "source docs/demos/prepare.sh"
Enter
Sleep 1s
Type "wg up --model code"
Enter
Wait+Screen /PID:/
Wait+Line /\$$/
Sleep 1s
Type "wg down"
Enter
Sleep 1s
Type "clear"
Enter
Sleep 500ms
Type "wg up --model code"
Show
Sleep 500ms
Enter
Wait+Screen /Text index:/
Wait+Line /\$$/
Sleep 500ms
Type `wg "How does the parser handle unary expressions in Rust code?" -k 1`
Enter
Sleep 3000ms
Type `wg --no-heading -n -F "struct Flags" .`
Enter
Sleep 2500ms
Hide
Type "wg down"
Enter
Sleep 1500ms
