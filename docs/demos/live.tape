Require wg
Require rg
Output docs/media/wg-live.gif
Output docs/media/wg-live.mp4
Set Shell "bash"
Set Width 1200
Set Height 800
Set FontSize 23
Set FontFamily "JetBrains Mono"
Set Theme "Catppuccin Mocha"
Set Padding 26
Set Margin 18
Set MarginFill "#1e1e2e"
Set BorderRadius 16
Set WindowBar Colorful
Set WindowBarSize 36
Set TypingSpeed 14ms
Set Framerate 30
Set PlaybackSpeed 1
Set WaitTimeout 2m
Hide
Type "source docs/demos/prepare.sh"
Enter
Sleep 500ms
Type "wg up --model code"
Enter
Wait+Line /❯\s*$/
Show
Type "# exact search: no match yet"
Enter
Type "wg -n -F refresh_token ."
Enter
Wait+Line /❯\s*$/
Sleep 700ms
Type "echo 'pub fn refresh_token() -> bool { true }' >> auth.rs"
Enter
Sleep 300ms
Type "# the daemon sees the edit immediately"
Enter
Type "wg -n -F refresh_token ."
Enter
Wait+Line /❯\s*$/
Sleep 1800ms
Hide
Type "wg down"
Enter
Wait+Line /❯\s*$/
