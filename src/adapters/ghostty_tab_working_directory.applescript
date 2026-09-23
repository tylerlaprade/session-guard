on run {tabId}
  tell application "Ghostty"
    repeat with w in windows
      repeat with t in tabs of w
        if id of t is tabId then return working directory of focused terminal of t
      end repeat
    end repeat
    return ""
  end tell
end run
