import 'package:desktop_drop/desktop_drop.dart';
import 'package:flutter/material.dart';

import '../../common.dart';
import '../../models/platform_model.dart';

/// Custom: pseudo-directory prefix put in front of a dropped file's name.
///
/// The controlling side cannot know the real desktop path of the controlled side, so it sends
/// `$DESKTOP$/name` and the controlled side resolves it to its own desktop. Keep in sync with
/// `CUSTOM_DROP_DESKTOP_PREFIX` in `src/server/connection.rs`.
const String kCustomDropDesktopPrefix = r'$DESKTOP$';

/// Uploads one locally dropped file to the peer's desktop.
///
/// Drives the stock upload path (`sessionSendFiles` with `isRemote: false`), so the peer stores
/// the file exactly like a normal transfer — only the destination path is a pseudo-directory.
/// The job id comes from the same allocator the file manager uses, so ids can never collide.
void sendDroppedFileToDesktop({
  required SessionID sessionId,
  required String localPath,
  required String name,
}) {
  bind.sessionSendFiles(
    sessionId: sessionId,
    actId: int.parse(bind.mainGetCommonSync(key: 'transfer-job-id')),
    path: localPath,
    to: '$kCustomDropDesktopPrefix/$name',
    fileNum: 0,
    includeHidden: false,
    isRemote: false,
    isDir: false,
  );
}

/// Makes files dragged from the local desktop droppable straight onto the remote screen.
///
/// When [enabled] is false the child is returned untouched, so sessions where uploading makes no
/// sense (view-only, peer is Android, ...) behave exactly as before.
class RemoteDropZone extends StatefulWidget {
  const RemoteDropZone({
    Key? key,
    required this.sessionId,
    required this.enabled,
    required this.child,
  }) : super(key: key);

  final SessionID sessionId;
  final bool enabled;
  final Widget child;

  @override
  State<RemoteDropZone> createState() => _RemoteDropZoneState();
}

class _RemoteDropZoneState extends State<RemoteDropZone> {
  bool _hovering = false;

  @override
  Widget build(BuildContext context) {
    if (!widget.enabled) {
      return widget.child;
    }
    return DropTarget(
      onDragEntered: (_) => _setHovering(true),
      onDragExited: (_) => _setHovering(false),
      onDragDone: (detail) {
        _setHovering(false);
        for (final file in detail.files) {
          sendDroppedFileToDesktop(
            sessionId: widget.sessionId,
            localPath: file.path,
            name: file.name,
          );
        }
      },
      child: Stack(
        children: [
          widget.child,
          if (_hovering) _buildHint(),
        ],
      ),
    );
  }

  void _setHovering(bool value) {
    if (_hovering != value && mounted) {
      setState(() => _hovering = value);
    }
  }

  Widget _buildHint() {
    return Positioned.fill(
      child: IgnorePointer(
        child: Container(
          color: const Color(0x66000000),
          alignment: Alignment.center,
          child: Container(
            padding: const EdgeInsets.symmetric(horizontal: 18, vertical: 12),
            decoration: const BoxDecoration(
              color: Color(0xE6262626),
              borderRadius: BorderRadius.all(Radius.circular(10)),
            ),
            child: Row(
              mainAxisSize: MainAxisSize.min,
              children: [
                const Icon(Icons.file_download_outlined,
                    size: 18, color: Colors.white),
                const SizedBox(width: 10),
                Text(
                  translate('Drop here to send to desktop'),
                  style: const TextStyle(color: Colors.white, fontSize: 13),
                ),
              ],
            ),
          ),
        ),
      ),
    );
  }
}
