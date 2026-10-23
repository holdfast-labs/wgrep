Require wg
Output docs/media/wg-live.mp4
Set Shell "bash"
Set Width 1200
Set Height 178
Set FontSize 26
Set FontFamily "DejaVu Sans Mono"
Set Theme "Catppuccin Mocha"
Set Padding 3
Set Margin 3
Set MarginFill "#181825"
Set BorderRadius 8
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
Type "clear"
Enter
Sleep 500ms
Show
Type `wg --no-heading -n -F round_secs .`
Enter
Sleep 1500ms
Type `echo 'pub fn round_secs(t:f64)->u64{t.round() as u64}' >> live.rs`
Enter
Sleep 800ms
Type `wg --no-heading -n -F round_secs .`
Enter
Sleep 2500ms
Hide
Type "wg down"
Enter
Sleep 1500ms
