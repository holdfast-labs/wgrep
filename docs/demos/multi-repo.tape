Require wg
Require rg
Output docs/media/wg-multi-repo.gif
Output docs/media/wg-multi-repo.mp4
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
Show
Type "wg up --model code"
Enter
Wait+Line /❯\s*$/
Sleep 700ms
Type "cd ../search-service"
Enter
Type "wg up --model code"
Enter
Wait+Line /❯\s*$/
Sleep 700ms
Type "wg 'decode packet length' -k 1"
Enter
Wait+Line /❯\s*$/
Sleep 1800ms
Hide
Type "wg down"
Enter
Wait+Line /❯\s*$/
