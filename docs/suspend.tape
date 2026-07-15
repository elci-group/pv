# pv suspend / resume — graceful freeze of a running process
Output docs/suspend.gif

Set Shell bash
Set FontSize 15
Set FontFamily "DejaVu Sans Mono"
Set Width 1100
Set Height 460
Set Theme "Catppuccin Mocha"
Set TypingSpeed 50ms
Set Framerate 30

Hide
Type "export PS1='$ '"
Enter
Type "clear"
Enter
Show

Type "sleep 600 &"
Sleep 300ms
Enter
Sleep 800ms

Type "pv suspend sleep"
Sleep 400ms
Enter
Sleep 1800ms

Type "pv resume sleep"
Sleep 400ms
Enter
Sleep 1800ms

Type "kill %1"
Sleep 300ms
Enter
Sleep 600ms
