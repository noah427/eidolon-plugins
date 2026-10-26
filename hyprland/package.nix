# The plugin's runtime package: the three clients its verbs run, in one bin directory.
# Nothing here keeps running — each verb runs its client for one act and the client exits —
# so this is a buildEnv and not a service, and the plugin declares no `service:` block.
{ lib, buildEnv, grim, wlrctl, wtype }:

buildEnv {
  name = "eidolon-hyprland-tools";
  paths = [ grim wlrctl wtype ];
  pathsToLink = [ "/bin" "/share/man" ];
  meta = {
    description = "grim, wlrctl and wtype for eidolon's hyprland plugin";
    longDescription = ''
      hyprland_screenshot runs grim; the input verbs (click, scroll) run wlrctl and
      (type, key) run wtype. hyprctl comes with Hyprland. Put this profile's bin on
      PATH and the plugin's verbs find all of them.
    '';
    license = lib.licenses.mit;
    platforms = lib.platforms.linux;
  };
}
