Require wg
Output docs/media/wg-multi-repo.mp4
Set Shell "bash"
Set Width 1200
Set Height 700
Set FontSize 25
Set FontFamily "DejaVu Sans Mono"
Set Theme "Catppuccin Mocha"
Set Padding 16
Set Margin 10
Set MarginFill "#181825"
Set BorderRadius 16
Set WindowBar Colorful
Set WindowBarSize 36
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
Type `until wg status --json | grep -q '"building":false'; do sleep 0.5; done`
Enter
Wait+Line /\$$/
Type "clear"
Enter
Sleep 500ms
Show
Type "cd ../api-server"
Enter
Sleep 500ms
Type "wg up --model code"
Enter
Sleep 1500ms
Type "wg status"
Enter
Sleep 3500ms
Hide
Type "wg down"
Enter
Sleep 1500ms
