Require wg
Require rg
Output docs/launch/renders/raw/wg-live-16x9.mp4
Set Shell "bash"
Set Width 1920
Set Height 560
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
Type `wg --no-heading -n -F round_secs .`
Enter
Sleep 2500ms
Type `echo 'pub fn round_secs(t:f64)->u64{t.round() as u64}' >> live.rs`
Enter
Sleep 1000ms
Type `wg --no-heading -n -F round_secs .`
Enter
Sleep 3000ms
Hide
Type "wg down"
Enter
Sleep 1500ms
