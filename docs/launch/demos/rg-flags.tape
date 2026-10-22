Require wg
Require rg
Output docs/launch/renders/raw/wg-rg-flags-16x9.mp4
Set Shell "bash"
Set Width 1920
Set Height 900
Set FontSize 41
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
Type "export P=zzqxjwvbnm_wg_text_absent_zzz"
Enter
Sleep 500ms
Type "wg -F $P ."
Enter
Sleep 1000ms
Type "time rg -F $P ."
Enter
Sleep 2500ms
Type "time wg -F $P ."
Enter
Sleep 2500ms
Type `wg --no-heading -n -F "struct Flags" .`
Enter
Sleep 2500ms
Hide
Type "wg down"
Enter
Sleep 1500ms
