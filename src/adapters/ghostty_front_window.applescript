on run
  if application "Ghostty" is not running then return ""
  tell application "Ghostty"
    if (count of windows) is 0 then return ""
    return id of front window
  end tell
end run
