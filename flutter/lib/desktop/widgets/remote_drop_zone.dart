import 'dart:async';
import 'dart:io';

import 'package:desktop_drop/desktop_drop.dart';
import 'package:flutter/material.dart';
import 'package:get/get.dart';

import '../../common.dart';
import '../../models/file_model.dart';
import '../../models/platform_model.dart';

/// Custom: pseudo-directory prefix put in front of a dropped file's name.
///
/// The controlling side cannot know the real desktop path of the controlled side, so it sends
/// `$DESKTOP$/name` and the controlled side resolves it to its own desktop. Keep in sync with
/// `CUSTOM_DROP_DESKTOP_PREFIX` in `src/server/connection.rs`.
const String kCustomDropDesktopPrefix = r'$DESKTOP$';

/// Uploads one locally dropped item to the peer's desktop, with a transfer job behind it.
///
/// Registering the job is what makes the drop *visible*. The stock file manager does exactly the
/// same thing before sending (`JobController.addTransferJob`): the id it hands out is the one the
/// Rust side streams `job_progress` / `job_done` events for, and `tryUpdateJobProgress` folds those
/// into `jobTable`. Without it the bytes still move, but nothing can be drawn for them.
///
/// Returns the job id, or `null` when the drop cannot be dispatched at all.
int? sendDroppedItemToDesktop({
  required SessionID sessionId,
  required JobController jobController,
  required String localPath,
  required String name,
}) {
  if (localPath.isEmpty || name.isEmpty) {
    debugPrint('Ignoring dropped item without a usable path/name: $localPath');
    return null;
  }

  final bool isDir;
  final int size;
  try {
    isDir = FileSystemEntity.isDirectorySync(localPath);
    // `Entry.entryType` only matters through its >3 / <3 split (file vs directory).
    size = isDir ? 0 : File(localPath).lengthSync();
  } catch (e) {
    debugPrint('Failed to stat dropped item $localPath: $e');
    return null;
  }

  final jobId = jobController.addTransferJob(
    Entry()
      ..path = localPath
      ..name = name
      ..entryType = isDir ? 1 : 4
      ..size = size,
    false,
  );

  if (isDir) {
    // A directory has no cheap size, and the progress bar needs a denominator. Summing the tree
    // synchronously would freeze the session window, so fill it in afterwards instead; until then
    // the bar simply renders as indeterminate.
    unawaited(_fillDirectorySize(jobController, jobId, localPath));
  }

  bind.sessionSendFiles(
    sessionId: sessionId,
    actId: jobId,
    path: localPath,
    to: '$kCustomDropDesktopPrefix/$name',
    fileNum: 0,
    includeHidden: false,
    isRemote: false,
    isDir: isDir,
  );
  return jobId;
}

/// Adds up a dropped directory so its progress bar has a total. Best-effort only: on failure the
/// bar stays indeterminate rather than blocking the transfer.
Future<void> _fillDirectorySize(
    JobController jobController, int jobId, String path) async {
  var total = 0;
  try {
    await for (final entity
        in Directory(path).list(recursive: true, followLinks: false)) {
      if (entity is File) {
        try {
          total += await entity.length();
        } catch (_) {
          // Unreadable entry: it simply does not add to the total.
        }
      }
    }
  } catch (e) {
    debugPrint('Failed to size dropped directory $path: $e');
    return;
  }
  final index = jobController.getJob(jobId);
  if (index < 0) return;
  jobController.jobTable[index].totalSize = total;
  jobController.jobTable.refresh();
}

/// Makes files dragged from the local desktop droppable straight onto the remote screen.
///
/// When [enabled] is false the child is returned untouched, so sessions where uploading makes no
/// sense (view-only, peer is Android, ...) behave exactly as before.
class RemoteDropZone extends StatefulWidget {
  const RemoteDropZone({
    Key? key,
    required this.sessionId,
    required this.jobController,
    required this.enabled,
    required this.child,
  }) : super(key: key);

  final SessionID sessionId;

  /// Owned by the session, so dropped transfers share the progress table the file manager uses
  /// instead of inventing a second one.
  final JobController jobController;
  final bool enabled;
  final Widget child;

  @override
  State<RemoteDropZone> createState() => _RemoteDropZoneState();
}

class _RemoteDropZoneState extends State<RemoteDropZone> {
  bool _hovering = false;

  /// Job ids created by drops in *this* window, so one session's panel never shows another's work.
  final _jobs = <int>{};
  Timer? _settleTimer;
  DateTime? _settledAt;

  @override
  void dispose() {
    _settleTimer?.cancel();
    _settleTimer = null;
    super.dispose();
  }

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
        var dispatched = 0;
        for (final file in detail.files) {
          final jobId = sendDroppedItemToDesktop(
            sessionId: widget.sessionId,
            jobController: widget.jobController,
            localPath: file.path,
            name: file.name,
          );
          if (jobId == null) continue;
          dispatched++;
          _jobs.add(jobId);
        }
        if (dispatched == 0) {
          // Nothing was even queued, so there will be no progress panel to explain it.
          showToast(translate('Failed'));
          return;
        }
        setState(() {});
        _ensureSettleTimer();
      },
      child: Stack(
        children: [
          widget.child,
          if (_hovering) _buildHint(),
          if (_jobs.isNotEmpty) _buildProgressPanel(),
        ],
      ),
    );
  }

  void _setHovering(bool value) {
    if (_hovering != value && mounted) {
      setState(() => _hovering = value);
    }
  }

  /// Retires the panel once every dropped job has settled, holding the finished state briefly so the
  /// outcome can actually be read (the transfer itself gives no other confirmation).
  void _ensureSettleTimer() {
    _settleTimer?.cancel();
    _settledAt = null;
    _settleTimer = Timer.periodic(const Duration(milliseconds: 400), (timer) {
      if (!mounted || _jobs.isEmpty) {
        timer.cancel();
        _settleTimer = null;
        return;
      }
      final table = widget.jobController.jobTable;
      final settled = _jobs.every((id) {
        final index = table.indexWhere((job) => job.id == id);
        if (index < 0) return true; // Job vanished: nothing left to wait for.
        final state = table[index].state;
        return state == JobState.done || state == JobState.error;
      });
      if (!settled) {
        _settledAt = null;
        return;
      }
      _settledAt ??= DateTime.now();
      if (DateTime.now().difference(_settledAt!) <
          const Duration(milliseconds: 1400)) {
        return;
      }
      timer.cancel();
      _settleTimer = null;
      _settledAt = null;
      if (mounted) {
        setState(_jobs.clear);
      }
    });
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

  /// Paste-style progress card, fed by the same `job_progress` events the file manager consumes.
  Widget _buildProgressPanel() {
    final table = widget.jobController.jobTable;
    return Positioned(
      right: 16,
      bottom: 16,
      child: Obx(() {
        final active = <JobProgress>[];
        var settledCount = 0;
        for (final id in _jobs) {
          final index = table.indexWhere((job) => job.id == id);
          if (index < 0) continue;
          final job = table[index];
          active.add(job);
          if (job.state == JobState.done) settledCount++;
        }
        if (active.isEmpty) return const SizedBox.shrink();
        return Container(
          width: 320,
          padding: const EdgeInsets.fromLTRB(14, 12, 14, 4),
          decoration: BoxDecoration(
            color: const Color(0xF21E1E1E),
            borderRadius: BorderRadius.circular(10),
            boxShadow: const [
              BoxShadow(
                  color: Color(0x55000000),
                  blurRadius: 14,
                  offset: Offset(0, 4)),
            ],
          ),
          child: Column(
            mainAxisSize: MainAxisSize.min,
            crossAxisAlignment: CrossAxisAlignment.start,
            children: [
              Row(
                children: [
                  Expanded(
                    child: Text(
                      translate('Sending to the remote desktop'),
                      style: const TextStyle(
                          color: Colors.white,
                          fontSize: 12.5,
                          fontWeight: FontWeight.w500),
                    ),
                  ),
                  Text(
                    '$settledCount/${active.length}',
                    style: const TextStyle(color: Colors.white54, fontSize: 11),
                  ),
                ],
              ),
              const SizedBox(height: 10),
              ConstrainedBox(
                constraints: const BoxConstraints(maxHeight: 220),
                child: SingleChildScrollView(
                  child: Column(
                    mainAxisSize: MainAxisSize.min,
                    children: active.map(_buildJobRow).toList(),
                  ),
                ),
              ),
            ],
          ),
        );
      }),
    );
  }

  Widget _buildJobRow(JobProgress job) {
    final done = job.state == JobState.done;
    final failed = job.state == JobState.error;
    final double? value = failed
        ? 1.0
        : done
            ? 1.0
            : (job.totalSize > 0 ? job.percent.clamp(0.0, 1.0).toDouble() : null);
    final color = failed
        ? const Color(0xFFE06C75)
        : done
            ? const Color(0xFF5FBF7F)
            : MyTheme.accent;
    final sizeText = job.totalSize > 0
        ? '${readableFileSize(job.finishedSize.toDouble())} / ${readableFileSize(job.totalSize.toDouble())}'
        : readableFileSize(job.finishedSize.toDouble());
    return Padding(
      padding: const EdgeInsets.only(bottom: 10),
      child: Column(
        crossAxisAlignment: CrossAxisAlignment.start,
        children: [
          Row(
            children: [
              Icon(
                done
                    ? Icons.check_circle_outline
                    : failed
                        ? Icons.error_outline
                        : Icons.arrow_upward,
                size: 13,
                color: color,
              ),
              const SizedBox(width: 6),
              Expanded(
                child: Text(
                  job.fileName.isEmpty ? job.jobName : job.fileName,
                  maxLines: 1,
                  overflow: TextOverflow.ellipsis,
                  style: const TextStyle(color: Colors.white, fontSize: 12),
                ),
              ),
              const SizedBox(width: 8),
              Text(
                failed
                    ? translate('Failed')
                    : done
                        ? translate('Finished')
                        : job.percentText,
                style: TextStyle(color: color, fontSize: 11),
              ),
            ],
          ),
          const SizedBox(height: 5),
          ClipRRect(
            borderRadius: BorderRadius.circular(3),
            child: LinearProgressIndicator(
              value: value,
              minHeight: 4,
              backgroundColor: const Color(0x26FFFFFF),
              valueColor: AlwaysStoppedAnimation<Color>(color),
            ),
          ),
          if (!done && !failed)
            Padding(
              padding: const EdgeInsets.only(top: 4),
              child: Text(
                job.speed > 0 ? '$sizeText   ${readableFileSize(job.speed)}/s' : sizeText,
                style: const TextStyle(color: Colors.white38, fontSize: 10.5),
              ),
            ),
        ],
      ),
    );
  }
}
