use framework "AppKit"

on run {bundleId}
  set appUrl to current application's NSWorkspace's sharedWorkspace()'s URLForApplicationWithBundleIdentifier:bundleId
  if appUrl is missing value then return ""
  return (appUrl's |path|()) as text
end run
