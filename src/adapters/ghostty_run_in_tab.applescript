on run {targetTty, commandText}
  tell application "Ghostty"
    repeat with ghosttyWindow in windows
      repeat with ghosttyTab in tabs of ghosttyWindow
        repeat with ghosttyTerminal in terminals of ghosttyTab
          if (tty of ghosttyTerminal) is targetTty then
            if (perform action ("text:\\x15" & commandText & "\\r") on ghosttyTerminal) then return "ran"
            return ""
          end if
        end repeat
      end repeat
    end repeat
    return ""
  end tell
end run
