Output demo.gif

Set Shell "bash"
Set FontSize 18
Set Width 1200
Set Height 600
Set Padding 12
Set Framerate 25
Set TypingSpeed 30ms
Set Theme "TokyoNight"

Type "blade --file examples/demo.blade"
Sleep 400ms
Enter
Sleep 4s

# Select API > Git pull and run it through the confirmation popup.
Down@120ms 4
Sleep 700ms
Enter
Sleep 900ms
Enter
Sleep 2s

# Jump to the dashboard and confirm a restart while its output is live.
Ctrl+P
Type@50ms "dashboard"
Sleep 700ms
Enter
Sleep 1s
Type "r"
Sleep 900ms
Enter
Sleep 4s

# Gracefully stop the demo stack and exit.
Type "q"
Sleep 900ms
Enter
Sleep 1s
