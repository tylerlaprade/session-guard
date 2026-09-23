on run {shellLine}
  tell application "Terminal"
    activate
    do script shellLine
  end tell
end run
