@tool
# The editor end of the godot bridge: a dial-out to the `eidolon-godot` host, and
# the verbs that answer for the scene this editor is editing.
#
# Two rules shape everything here, and they are the plugin's, not this file's.
#
# **The editor dials; nothing dials the editor.** Godot cannot be started by
# eidolon the way a headless browser can, so the connection goes this way and the
# addon reconnects forever: the host may be started after the editor, restarted
# under it, or not exist yet, and none of that is a state a person has to fix.
#
# **A call that cannot be done is a sentence, not an error code.** Every refusal
# below names what is missing and what would make it appear, because the thing
# reading it is a model that has to decide what to do next, and "invalid argument"
# decides nothing.
#
# The wire is newline-delimited JSON over one loopback TCP socket, and the host is
# `../service`. This half owns no state a second call can disturb: the host hands
# out the ids, so a reply is a function of the request it answers.
extends Node

# ---------------------------------------------------------------------------
# The wire.
# ---------------------------------------------------------------------------

const PROTOCOL := 1

# How often a tick goes out while attached. It is the host's only way to tell an
# editor that is busy from one that is gone: a killed engine sends no FIN and no
# close, it simply stops running frames, so silence is the signal.
const TICK_MS := 2000

# How long to wait before dialling again after a failure, a refusal or a close.
# Small enough that a host started later is picked up promptly, large enough that a
# host that is not there costs nothing.
const RECONNECT_MS := 2000

# A line longer than this is not a message this bridge sends or expects; dropping
# the connection is better than growing a buffer in somebody's editor.
const MAX_BUFFER := 1 << 20

# Set by `plugin.gd` before this node is added to the tree. It is the whole reason
# this is a child rather than the plugin itself.
var editor_plugin: EditorPlugin = null

var host := "127.0.0.1"
var port := 8094

var _peer: StreamPeerTCP = null
var _buf := PackedByteArray()
var _token := ""
var _hello_sent := false
var _attached := false
var _next_attempt := 0
var _next_tick := 0
var _last_error := ""

func _ready() -> void:
	set_process(true)
	var bridge_port := OS.get_environment("EIDOLON_GODOT_BRIDGE_PORT")
	if bridge_port != "" and bridge_port.is_valid_int():
		port = bridge_port.to_int()
	var bridge_host := OS.get_environment("EIDOLON_GODOT_HOST")
	if bridge_host != "":
		host = bridge_host

func stop() -> void:
	_hang_up("")

func _exit_tree() -> void:
	_hang_up("")

# ---------------------------------------------------------------------------
# The connection: dial, hello, tick, read.
# ---------------------------------------------------------------------------

func _process(_delta: float) -> void:
	var now := Time.get_ticks_msec()
	if _peer == null:
		if now >= _next_attempt:
			_next_attempt = now + RECONNECT_MS
			_dial()
		return

	_peer.poll()
	var status := _peer.get_status()
	if status == StreamPeerTCP.STATUS_ERROR or status == StreamPeerTCP.STATUS_NONE:
		_hang_up("the bridge closed the connection")
		return
	if status != StreamPeerTCP.STATUS_CONNECTED:
		return

	if not _hello_sent:
		_hello_sent = true
		_say_hello()
	_read()
	if _attached and now >= _next_tick:
		_next_tick = now + TICK_MS
		_send({"tick": 1})

func _dial() -> void:
	if _token == "":
		# Tried again here rather than once at startup: the host makes the token
		# file the first time it runs, and an editor that started first must pick
		# it up without anybody restarting anything.
		_token = _read_token()
	if _token == "":
		return
	_peer = StreamPeerTCP.new()
	var err := _peer.connect_to_host(host, port)
	if err != OK:
		_last_error = "could not dial %s:%d: %s" % [host, port, error_string(err)]
		_peer = null
		return
	_buf = PackedByteArray()
	_hello_sent = false
	_attached = false

func _hang_up(why: String) -> void:
	if _peer != null:
		_peer.disconnect_from_host()
	_peer = null
	_buf = PackedByteArray()
	_hello_sent = false
	_attached = false
	_next_attempt = Time.get_ticks_msec() + RECONNECT_MS
	# Said once per distinct reason, not once per attempt: a bridge that is not
	# running is the normal state of an editor nobody is asking anything of.
	if why != "" and why != _last_error:
		printerr("[eidolon] %s" % why)
	_last_error = why

func _say_hello() -> void:
	_send({
		"hello": {
			"protocol": PROTOCOL,
			"token": _token,
			"engine": Engine.get_version_info().string,
			"project": ProjectSettings.globalize_path("res://"),
			"project_name": str(ProjectSettings.get_setting("application/config/name", "")),
		},
	})

func _send(message: Dictionary) -> void:
	if _peer == null or _peer.get_status() != StreamPeerTCP.STATUS_CONNECTED:
		return
	var line := JSON.stringify(message) + "\n"
	_peer.put_data(line.to_utf8_buffer())

func _read() -> void:
	var available := _peer.get_available_bytes()
	if available > 0:
		var got := _peer.get_data(available)
		if got[0] != OK:
			_hang_up("reading from the bridge failed: %s" % error_string(got[0]))
			return
		_buf.append_array(got[1])
	if _buf.size() > MAX_BUFFER:
		_hang_up("the bridge sent more than %d bytes with no newline" % MAX_BUFFER)
		return
	while true:
		var at := _buf.find(10)
		if at < 0:
			return
		var line := _buf.slice(0, at).get_string_from_utf8()
		_buf = _buf.slice(at + 1)
		if line.strip_edges() != "":
			_handle(line)

func _handle(line: String) -> void:
	var message = JSON.parse_string(line)
	if typeof(message) != TYPE_DICTIONARY:
		printerr("[eidolon] the bridge sent a line that is not a JSON object; ignoring it")
		return
	if message.has("welcome"):
		_attached = true
		_last_error = ""
		return
	if message.has("bye"):
		_hang_up("the bridge refused this editor: %s" % str(message["bye"]))
		return
	if not message.has("id"):
		# A tick, or something this half does not know about. Silently: the host
		# logging one unexplained line per frame is worse than nothing.
		return
	var id := int(message["id"])
	var reply := _dispatch(str(message.get("method", "")), message.get("args", {}))
	if typeof(reply) != TYPE_DICTIONARY or not reply.has("ok"):
		reply = _fail("the handler for `%s` returned nothing this addon can send" % message.get("method", ""))
	reply["id"] = id
	_send(reply)

# ---------------------------------------------------------------------------
# The verbs. One function per method the host forwards.
# ---------------------------------------------------------------------------

func _dispatch(method: String, args) -> Dictionary:
	if typeof(args) != TYPE_DICTIONARY:
		return _fail("`args` must be an object")
	match method:
		"status":
			return _do_status(args)
		"tree":
			return _do_tree(args)
		"node":
			return _do_node(args)
		"set":
			return _do_set(args)
		"call":
			return _do_call(args)
		"scene":
			return _do_scene(args)
		"run":
			return _do_run(args)
	return _fail("this addon has no `%s`; the host asked for a method it does not know" % method)

func _do_status(_args: Dictionary) -> Dictionary:
	var editor := _editor()
	var root := editor.get_edited_scene_root()
	var selected := []
	var selection = editor.get_selection()
	if selection != null:
		for node in selection.get_selected_nodes():
			if node is Node:
				selected.append(String(node.name))
	return _ok({
		"engine": Engine.get_version_info().string,
		"project": ProjectSettings.globalize_path("res://"),
		"project_name": str(ProjectSettings.get_setting("application/config/name", "")),
		"main_scene": str(ProjectSettings.get_setting("application/run/main_scene", "")),
		"edited_scene": "" if root == null else str(root.scene_file_path),
		"edited_root": "" if root == null else String(root.name),
		"open_scenes": _strings(editor.get_open_scenes()),
		"unsaved_scenes": _strings(editor.get_unsaved_scenes()),
		"playing": editor.is_playing_scene(),
		"selected": selected,
	})

func _do_tree(args: Dictionary) -> Dictionary:
	var at := _at(args)
	if not at["ok"]:
		return at
	var root: Node = at["root"]
	var start: Node = at["node"]
	var depth := _int_arg(args, "depth", 2)
	var most := clampi(_int_arg(args, "max", 300), 1, 5000)
	var nodes := []
	var queue: Array = [[start, 0]]
	var truncated := false
	while not queue.is_empty():
		if nodes.size() >= most:
			truncated = true
			break
		var item: Array = queue.pop_front()
		var node: Node = item[0]
		var level: int = item[1]
		nodes.append(_brief(root, node))
		if depth >= 0 and level >= depth:
			continue
		for child in node.get_children(false):
			queue.append([child, level + 1])
	return _ok({
		"scene": String(root.scene_file_path),
		"root": String(root.name),
		"start": _rel(root, start),
		"depth": depth,
		"count": nodes.size(),
		"truncated": truncated,
		"nodes": nodes,
	})

func _do_node(args: Dictionary) -> Dictionary:
	var at := _at(args)
	if not at["ok"]:
		return at
	var root: Node = at["root"]
	var node: Node = at["node"]
	var everything := bool(args.get("all", false))
	var wanted := str(args.get("filter", "")).strip_edges().to_lower()
	var most := clampi(_int_arg(args, "max", 200), 1, 2000)
	var script: Script = node.get_script() as Script

	var properties := []
	var truncated := false
	for entry in node.get_property_list():
		var name := String(entry["name"])
		var usage: int = entry["usage"]
		if usage & _USAGE_NOT_A_VALUE != 0:
			continue
		if not everything and usage & PROPERTY_USAGE_SCRIPT_VARIABLE == 0:
			continue
		if usage & PROPERTY_USAGE_EDITOR == 0:
			continue
		if wanted != "" and not name.to_lower().contains(wanted):
			continue
		if properties.size() >= most:
			truncated = true
			break
		properties.append({
			"name": name,
			"type": type_string(typeof(node.get(name))),
			"exported": usage & PROPERTY_USAGE_SCRIPT_VARIABLE != 0,
			"value": _value_record(node.get(name)),
		})

	var children := []
	for child in node.get_children(false):
		children.append({
			"path": _rel(root, child),
			"name": String(child.name),
			"class": child.get_class(),
		})

	var out := _brief(root, node)
	out["groups"] = _strings(node.get_groups())
	out["children"] = children
	out["properties"] = properties
	out["properties_truncated"] = truncated
	out["properties_scope"] = "everything the inspector shows" if everything else "the node's own script variables (all: true for the rest)"
	out["signals"] = _signals(node, script, everything)
	out["methods"] = _methods(node, script)
	out["note"] = "hand the fingerprint to `godot_set` or `godot_call`; they refuse a node that is no longer this one"
	return _ok(out)

func _do_set(args: Dictionary) -> Dictionary:
	var at := _at(args)
	if not at["ok"]:
		return at
	var node: Node = at["node"]
	var root: Node = at["root"]
	var guard := _guarded(args, node)
	if not guard["ok"]:
		return guard

	var property := str(args.get("property", "")).strip_edges()
	if property == "":
		return _fail("`property` is required: the name of the thing to set, as `godot_node` lists it")
	if not _has_property(node, property):
		return _fail("%s has no property \"%s\". What it does carry: %s" % [node.get_class(), property, _near_properties(node, property)])

	var before: Variant = node.get(property)
	if before is Object:
		return _fail("\"%s\" holds an Object (%s). This verb sets values, not references: assigning a Resource or a node into somebody's scene is a structural edit, and the operator's own inspector is where that belongs" % [property, before.get_class()])
	var parsed := _parse_value(args)
	if not parsed["ok"]:
		return parsed
	var value: Variant = parsed["value"]

	var undo := _editor().get_editor_undo_redo()
	if undo == null:
		return _fail("the editor's undo history is not available right now, and this verb will not change a scene the operator cannot take back")
	undo.create_action("Eidolon: set %s.%s" % [String(node.name), property])
	undo.add_do_property(node, property, value)
	undo.add_undo_property(node, property, before)
	undo.commit_action()

	var after: Variant = node.get(property)
	var matched := var_to_str(after) == var_to_str(value)
	return _ok({
		"node": _rel(root, node),
		"fingerprint": _fingerprint(node),
		"property": property,
		"before": _value_record(before),
		"requested": _value_record(value),
		"after": _value_record(after),
		"matched": matched,
		"note": "" if matched else "the property does not hold what was asked for: a setter may clamp or transform it, and both readings are above. The scene is marked unsaved either way",
	})

func _do_call(args: Dictionary) -> Dictionary:
	var at := _at(args)
	if not at["ok"]:
		return at
	var node: Node = at["node"]
	var root: Node = at["root"]
	var guard := _guarded(args, node)
	if not guard["ok"]:
		return guard

	var method := str(args.get("method", "")).strip_edges()
	if method == "":
		return _fail("`method` is required: the name of the method to call on the node")
	if _DENIED.has(method):
		return _fail("`%s` is one of the three methods this verb will not run (%s): they take a node out of the scene, and a scene tree with a hole in it is not something a call should be able to leave behind. The operator deletes nodes" % [method, ", ".join(_DENIED)])
	if not node.has_method(method):
		var script: Script = node.get_script() as Script
		var names := _method_names(script, false)
		return _fail("%s has no method \"%s\".%s" % [
			node.get_class(),
			method,
			" Its script declares: %s" % ", ".join(names) if not names.is_empty() else "",
		])

	var raw = args.get("args", [])
	if typeof(raw) != TYPE_ARRAY:
		return _fail("`args` is the array of arguments to pass to the method, in order; for a method that takes a Vector2, `[{\"var\": \"Vector2(1, 2)\"}]` names one")
	var call_args := []
	for item in raw:
		call_args.append(_literal(item))

	var returned: Variant = node.callv(method, call_args)
	return _ok({
		"node": _rel(root, node),
		"fingerprint": _fingerprint(node),
		"method": method,
		"returned": _value_record(returned),
		"note": "a method call is not an editor action: it is not in the undo history, so the operator cannot take it back with Ctrl-Z, and the scene is not marked unsaved on its behalf",
	})

func _do_scene(args: Dictionary) -> Dictionary:
	var editor := _editor()
	var wants_open := args.has("open")
	var wants_save := bool(args.get("save", false))
	if wants_open == wants_save:
		return _fail("name exactly one of `open` (a scene to open: \"res://scenes/Main.tscn\") or `save: true` (write the scene being edited back to its file)")

	if wants_save:
		var root := editor.get_edited_scene_root()
		if root == null:
			return _fail("no scene is open in the editor, so there is nothing to save")
		if root.scene_file_path == "":
			return _fail("the edited scene has never been written to a file, so the editor would have to ask the operator where to put it — and this verb will not open a dialog on somebody's screen. Once the operator has saved it, this verb can")
		var err := editor.save_scene()
		if err != OK:
			return _fail("the editor would not save %s: %s" % [root.scene_file_path, error_string(err)])
		return _ok({
			"saved": str(root.scene_file_path),
			"unsaved_scenes": _strings(editor.get_unsaved_scenes()),
		})

	var path := str(args["open"]).strip_edges()
	if not path.begins_with("res://"):
		return _fail("`open` is a scene in this project, written the way Godot writes it — \"res://scenes/Main.tscn\" — not a filesystem path (%s). What is here: %s" % [path, ", ".join(_strings(editor.get_open_scenes()))])
	if not ResourceLoader.exists(path, "PackedScene"):
		return _fail("there is no scene at %s in this project. A project path is case-sensitive and has no leading slash; `godot_status` lists the scenes the editor already has open" % path)
	var unsaved := _strings(editor.get_unsaved_scenes())
	if not unsaved.is_empty():
		return _fail("the editor has unsaved changes in %s, and opening another scene discards them. This verb will not throw away the operator's edits behind a dialog they never saw: save first (`godot_scene` with save: true) or ask them to" % ", ".join(unsaved))

	editor.open_scene_from_path(path)
	var opened := editor.get_edited_scene_root()
	var now := "" if opened == null else str(opened.scene_file_path)
	return _ok({
		"requested": path,
		"opened": now,
		"matched": now == path,
		"root": "" if opened == null else String(opened.name),
		"fingerprint": "" if opened == null else _fingerprint(opened),
	})

func _do_run(args: Dictionary) -> Dictionary:
	var editor := _editor()
	var action := str(args.get("action", "")).strip_edges()
	if action == "stop":
		if not editor.is_playing_scene():
			return _fail("the editor is not playing this project, so there is nothing to stop")
		editor.stop_playing_scene()
		var stopped := not editor.is_playing_scene()
		return _ok({
			"requested": "stop",
			"playing": editor.is_playing_scene(),
			"matched": stopped,
			"note": "" if stopped else "the editor still reports the project as playing; the run may be shutting down, or it did not stop",
		})
	if action != "play":
		return _fail("`action` is \"play\" or \"stop\"; got %s" % ("nothing" if action == "" else "\"%s\"" % action))
	if editor.is_playing_scene():
		return _fail("the editor is already playing this project. `action: \"stop\"` first — a second play would leave the first run in a window nobody is looking at")

	var scene := str(args.get("scene", "main")).strip_edges()
	match scene:
		"main":
			var main := str(ProjectSettings.get_setting("application/run/main_scene", ""))
			if main == "":
				return _fail("this project has no main scene set (application/run/main_scene), so there is nothing to play. Name one: {\"action\": \"play\", \"scene\": \"res://...\"}")
			editor.play_main_scene()
		"current":
			editor.play_current_scene()
		_:
			if not scene.begins_with("res://"):
				return _fail("`scene` is \"main\", \"current\", or a scene in this project written the way Godot writes it (\"res://scenes/Main.tscn\"); got %s" % scene)
			if not ResourceLoader.exists(scene, "PackedScene"):
				return _fail("there is no scene at %s in this project, so there is nothing to play" % scene)
			editor.play_custom_scene(scene)

	var playing := editor.is_playing_scene()
	return _ok({
		"requested": "play",
		"scene": scene,
		"playing": playing,
		"matched": playing,
		"note": "playing starts a second process — a window on the operator's screen, or a headless run if the editor is headless. `action: \"stop\"` ends it, and closing the editor ends it too",
	})

# ---------------------------------------------------------------------------
# Resolving what a call is aimed at.
# ---------------------------------------------------------------------------

# The one place a path becomes a node, so that every verb refuses a missing one in
# the same words.
#
# Paths are relative to the *edited scene's* root, which is not where it looks: in
# an editor the scene is instantiated inside the editor's own viewport, under a path
# like /root/@EditorNode@19507/.../SubViewport, so `/root/Main/Player` names nothing.
# The root itself is ".", and "Player/Sprite" is a child of it.
func _at(args: Dictionary) -> Dictionary:
	var editor := _editor()
	var root := editor.get_edited_scene_root()
	if root == null:
		return _fail("no scene is open in the editor, so there is no tree to look at. Open one with `godot_scene` ({\"open\": \"res://...\"}), or have the operator open it")
	var path := str(args.get("path", ".")).strip_edges()
	while path.begins_with("./"):
		path = path.substr(2)
	if path.begins_with("/"):
		return _fail("`path` is relative to the edited scene's root, not to the editor's own tree: %s is not one. The root itself is \".\"; its children are \"Player\", \"Player/Sprite\"" % path)
	if path == "":
		path = "."
	var node: Node = root if path == "." else root.get_node_or_null(NodePath(path))
	if node == null:
		return _fail("no node at \"%s\" under the edited scene root. %s" % [path, _near_nodes(root, path)])
	return {"ok": true, "root": root, "node": node, "path": path}

# The identity a mutating verb has to be handed before it will touch anything.
# `get_instance_id` is unique for the life of the process and is not reused, so a
# node that was deleted and replaced at the same path is a different node — which
# is exactly the case a path alone cannot tell you about.
func _fingerprint(node: Node) -> String:
	return "%d|%s|%s" % [node.get_instance_id(), node.get_class(), _script_path(node)]

func _guarded(args: Dictionary, node: Node) -> Dictionary:
	var expect := str(args.get("expect", "")).strip_edges()
	if expect == "":
		return _fail("`expect` is required: the fingerprint `godot_node` reported for this node. It is what stops a call landing on whatever is at that path now — a scene changes under you whenever the operator edits it")
	var now := _fingerprint(node)
	if expect != now:
		return _fail("the node at that path is not the one that was inspected: expect %s, and it is now %s. Nothing was changed; run `godot_node` again for the current fingerprint" % [expect, now])
	return {"ok": true}

func _near_nodes(root: Node, path: String) -> String:
	var parts := path.split("/")
	var parent: Node = root
	if parts.size() > 1:
		parent = root.get_node_or_null(NodePath("/".join(parts.slice(0, parts.size() - 1))))
	if parent == null:
		return "`godot_tree` lists what the scene does hold."
	var names := []
	for child in parent.get_children(false):
		names.append(String(child.name))
	if names.is_empty():
		return "%s has no children." % _rel(root, parent)
	return "What that parent holds: %s." % ", ".join(names)

# ---------------------------------------------------------------------------
# Values: what a property is, and what the caller wants it to be.
# ---------------------------------------------------------------------------

const _USAGE_NOT_A_VALUE := PROPERTY_USAGE_CATEGORY | PROPERTY_USAGE_GROUP | PROPERTY_USAGE_INTERNAL

# Three methods that take a node out of the tree. The list is short on purpose: a
# verb that calls methods is not a verb that curates an API, and everything else a
# node can be asked to do the operator can undo by hand or reload the scene to
# escape. These three leave a hole that nothing repairs.
const _DENIED := ["free", "queue_free", "replace_by"]

# A property as three readings rather than one value: the Godot type, the literal
# the editor's inspector would show (`repr`, which `value_text` can read back), and
# the raw value when JSON can carry it honestly — which is scalars and strings and
# nothing else, because a Vector2 that becomes the string "(1, 2)" on the way
# through JSON is a value that quietly changed shape.
func _value_record(value: Variant) -> Dictionary:
	var record := {"type": type_string(typeof(value)), "repr": _repr(value)}
	match typeof(value):
		TYPE_NIL, TYPE_BOOL, TYPE_INT, TYPE_FLOAT, TYPE_STRING:
			record["value"] = value
		TYPE_STRING_NAME:
			record["value"] = String(value)
	return record

func _repr(value: Variant) -> String:
	var text := var_to_str(value)
	if text.length() > 4000:
		return text.substr(0, 4000) + "… (truncated at 4000 characters)"
	return text

func _has_property(node: Node, property: String) -> bool:
	for entry in node.get_property_list():
		if String(entry["name"]) == property and int(entry["usage"]) & _USAGE_NOT_A_VALUE == 0:
			return true
	return false

func _near_properties(node: Node, property: String) -> String:
	var needle := property.to_lower()
	var hits := []
	for entry in node.get_property_list():
		var name := String(entry["name"])
		if int(entry["usage"]) & _USAGE_NOT_A_VALUE != 0:
			continue
		var lower := name.to_lower()
		if lower.contains(needle) or needle.contains(lower):
			hits.append(name)
	if hits.is_empty():
		return "`godot_node` lists all of them"
	return ", ".join(hits.slice(0, 12))

# One argument as the Godot value it names.
func _literal(item: Variant) -> Variant:
	if typeof(item) == TYPE_DICTIONARY and item.has("var") and typeof(item["var"]) == TYPE_STRING:
		var parsed = str_to_var(item["var"])
		return parsed if parsed != null or item["var"].strip_edges() == "null" else item["var"]
	return item

func _parse_value(args: Dictionary) -> Dictionary:
	var has_value := args.has("value")
	var has_text := args.has("value_text")
	if has_value == has_text:
		return _fail("name exactly one of `value` (a JSON value: a number, a string, a bool) or `value_text` (a Godot literal, written the way the inspector shows it: \"Vector2(1, 2)\", \"Color(1, 0, 0)\", \"&\\\"idle\\\"\"): %s" % ("both were named" if has_value else "neither was named"))
	if has_text:
		var text := str(args["value_text"]).strip_edges()
		var parsed = str_to_var(text)
		if parsed == null and text != "null":
			return _fail("\"%s\" is not a Godot literal this engine can read. A plain string is not one — put it in `value` — and a literal is spelled the way `godot_node` reports it, e.g. Vector2(1, 2)" % text)
		return {"ok": true, "value": parsed}
	return {"ok": true, "value": _coerce(args["value"])}

# JSON has numbers, strings, bools, arrays and objects. Godot has those and thirty
# more types, and `set` refuses a value of the wrong one. So a JSON value is made
# into the *shape the property already has*: a Dictionary with x and y into the
# Vector2 the property is, an int into the float it is. What cannot be inferred is
# left alone and reported as not matching, which is a reading, not a guess.
func _coerce(value: Variant) -> Variant:
	if typeof(value) == TYPE_DICTIONARY:
		if value.has("x") and value.has("y") and value.has("z") and value.has("w"):
			return Vector4(value["x"], value["y"], value["z"], value["w"])
		if value.has("x") and value.has("y") and value.has("z"):
			return Vector3(value["x"], value["y"], value["z"])
		if value.has("x") and value.has("y"):
			return Vector2(value["x"], value["y"])
		if value.has("r") and value.has("g") and value.has("b"):
			return Color(value["r"], value["g"], value["b"], value.get("a", 1.0))
	if typeof(value) == TYPE_ARRAY:
		if value.size() == 4:
			return Vector4(value[0], value[1], value[2], value[3])
		if value.size() == 3:
			return Vector3(value[0], value[1], value[2])
		if value.size() == 2:
			return Vector2(value[0], value[1])
	return value

# ---------------------------------------------------------------------------
# The script attached to a node, which is where a project's own names live.
# ---------------------------------------------------------------------------

func _script_path(node: Node) -> String:
	var script := node.get_script()
	return "" if script == null else str(script.resource_path)

func _signal_names(entry: Dictionary) -> Array:
	var names := []
	for arg in entry.get("args", []):
		names.append(str(arg.get("name", "")))
	return names

func _signals(node: Node, script: Script, everything: bool) -> Array:
	var listed: Array = node.get_signal_list() if everything or script == null else script.get_script_signal_list()
	var out := []
	for entry in listed:
		var name := String(entry["name"])
		out.append({
			"name": name,
			"args": _signal_names(entry),
			"connections": node.get_signal_connection_list(name).size(),
		})
	return out

func _method_names(script: Script, everything: bool) -> Array:
	if script == null:
		return []
	var listed: Array = script.get_script_method_list()
	var out := []
	for entry in listed:
		out.append(String(entry["name"]))
	return out

func _methods(node: Node, script: Script) -> Array:
	var out := []
	for entry in (script.get_script_method_list() if script != null else []):
		var args := []
		for arg in entry.get("args", []):
			args.append(str(arg.get("name", "")))
		out.append({"name": String(entry["name"]), "args": args})
	return out

# ---------------------------------------------------------------------------
# Small shared pieces.
# ---------------------------------------------------------------------------

func _brief(root: Node, node: Node) -> Dictionary:
	var owner_name := ""
	if node.owner != null:
		owner_name = String(node.owner.name)
	return {
		"path": _rel(root, node),
		"name": String(node.name),
		"class": node.get_class(),
		"script": _script_path(node),
		"scene": str(node.scene_file_path),
		"children": node.get_child_count(false),
		"owner": owner_name,
		"fingerprint": _fingerprint(node),
	}

func _rel(root: Node, node: Node) -> String:
	return "." if node == root else String(root.get_path_to(node))

func _editor() -> EditorInterface:
	return editor_plugin.get_editor_interface()

func _strings(values) -> Array:
	var out := []
	for value in values:
		out.append(str(value))
	return out

func _int_arg(args: Dictionary, name: String, fallback: int) -> int:
	var value = args.get(name, fallback)
	if typeof(value) == TYPE_INT or typeof(value) == TYPE_FLOAT:
		return int(value)
	if typeof(value) == TYPE_STRING and value.strip_edges().is_valid_int():
		return value.strip_edges().to_int()
	return fallback

func _ok(result: Dictionary) -> Dictionary:
	return {"ok": true, "result": result}

func _fail(why: String) -> Dictionary:
	return {"ok": false, "error": why}

func _read_token() -> String:
	var path := _token_path()
	if not FileAccess.file_exists(path):
		return ""
	var file := FileAccess.open(path, FileAccess.READ)
	if file == null:
		printerr("[eidolon] cannot read the bridge token at %s: %s" % [path, error_string(FileAccess.get_open_error())])
		return ""
	var token := file.get_as_text().strip_edges()
	file.close()
	return token

func _token_path() -> String:
	var override := OS.get_environment("EIDOLON_GODOT_TOKEN_FILE")
	if override != "":
		return override
	return OS.get_environment("HOME").path_join(".config/eidolon/godot.token")
