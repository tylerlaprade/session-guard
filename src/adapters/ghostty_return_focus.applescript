on run {previousWindowId, frontAppId}
  if previousWindowId is not "" then
    tell application "Ghostty"
      if (count of windows) > 0 and id of front window is not previousWindowId then
        repeat with candidate in windows
          if id of candidate is previousWindowId then activate window candidate
        end repeat
      end if
    end tell
  end if
  if frontAppId is not "" then tell application id frontAppId to activate
end run
