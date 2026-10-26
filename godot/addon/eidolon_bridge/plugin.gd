@tool
# `eidolon_bridge`: the editor half of the godot plugin.
#
# An EditorPlugin is the only kind of script Godot lets act on the editor rather
# than on a running game, so this is where the plugin has to live. It does nothing
# but own one child and hand that child the editor interface: everything a call
# can actually do is in `bridge.gd`, which is a plain Node with `_process`, because
# an EditorPlugin's own `_process` is the editor's and a child with its own is
# easier to reason about (and to lift out later).
#
# Nothing here is automatic. Enabling the addon in Project > Project Settings >
# Plugins is what lends an editor to the bridge, and disabling it — or closing the
# editor — is what takes it back.
extends EditorPlugin

const Bridge := preload("bridge.gd")

var _bridge: Node = null

func _enter_tree() -> void:
	_bridge = Bridge.new()
	_bridge.name = "EidolonBridge"
	# Passed, not looked up: a plain Node has no `get_editor_interface()`, and
	# reaching for the `EditorInterface` singleton instead would work right up
	# until somebody runs this script somewhere that is not the editor.
	_bridge.editor_plugin = self
	add_child(_bridge)
	print("[eidolon] bridge addon enabled; dialling the eidolon-godot host")

func _exit_tree() -> void:
	if _bridge != null and is_instance_valid(_bridge):
		_bridge.stop()
		_bridge.queue_free()
		_bridge = null
	print("[eidolon] bridge addon disabled; the host will see the socket close")
