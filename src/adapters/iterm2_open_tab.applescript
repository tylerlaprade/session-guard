on run {shellLine}
  tell application "iTerm2"
    activate
    if (count of windows) = 0 then
      create window with default profile
      tell current session to write text shellLine
    else
      tell current window
        create tab with default profile
        tell current session to write text shellLine
      end tell
    end if
  end tell
end run
