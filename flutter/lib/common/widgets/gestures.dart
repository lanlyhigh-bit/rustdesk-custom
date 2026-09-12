import 'dart:async';
import 'package:flutter/gestures.dart';
import 'package:flutter/widgets.dart';
import 'package:flutter_hbb/common/widgets/remote_input.dart';

enum GestureState {
  none,
  oneFingerPan,
  twoFingerPending,
  twoFingerScroll,
  twoFingerScale,
  threeFingerVerticalDrag
}

// Two-finger gesture discrimination thresholds (gestures.dart).
// `focalPointDelta` is expressed in logical pixels.
// `scale` is a unitless ratio relative to the initial span (1.0 == no change).
// Scale changes above the threshold win over scroll; otherwise a dominant
// vertical focal-point movement above the threshold becomes a scroll.
const double kTwoFingerScaleThreshold = 0.05;
const double kTwoFingerScrollThreshold = 15.0;
const double kTwoFingerDirectionRatio = 1.5;

// Total focal-point travel (|dx| + |dy|) after which a two-finger gesture that
// never became clearly vertical or clearly horizontal is committed to pan.
// Without this fallback a slow or diagonal drag stayed in `twoFingerPending`
// forever and the canvas could not be moved at all.
const double kTwoFingerPanFallbackThreshold = 50.0;

class CustomTouchGestureRecognizer extends ScaleGestureRecognizer {
  CustomTouchGestureRecognizer({
    Object? debugOwner,
    Set<PointerDeviceKind>? supportedDevices,
  }) : super(
          debugOwner: debugOwner,
          supportedDevices: supportedDevices,
        ) {
    _init();
  }

  // oneFingerPan
  GestureDragStartCallback? onOneFingerPanStart;
  GestureDragUpdateCallback? onOneFingerPanUpdate;
  GestureDragEndCallback? onOneFingerPanEnd;
  GestureDragCancelCallback? onOneFingerPanCancel;

  // twoFingerScale : scale + pan event
  GestureScaleStartCallback? onTwoFingerScaleStart;
  GestureScaleUpdateCallback? onTwoFingerScaleUpdate;
  GestureScaleEndCallback? onTwoFingerScaleEnd;

  // twoFingerScroll : two-finger vertical drag as remote scroll
  GestureDragStartCallback? onTwoFingerScrollStart;
  GestureDragUpdateCallback? onTwoFingerScrollUpdate;
  GestureDragEndCallback? onTwoFingerScrollEnd;

  // When true (default), two fingers are discriminated between pinch-zoom and
  // vertical scroll. When false (legacy "threeFinger" mode), two fingers always
  // scale and three fingers scroll, matching the original behavior.
  bool twoFingerScrollEnabled = true;

  // Pending-state accumulators for two-finger discrimination.
  double _twoFingerStartScale = 1.0;
  double _twoFingerCumDx = 0.0;
  double _twoFingerCumDy = 0.0;

  // threeFingerVerticalDrag
  GestureDragStartCallback? onThreeFingerVerticalDragStart;
  GestureDragUpdateCallback? onThreeFingerVerticalDragUpdate;
  GestureDragEndCallback? onThreeFingerVerticalDragEnd;

  var _currentState = GestureState.none;
  Timer? _debounceTimer;

  void _init() {
    debugPrint("CustomTouchGestureRecognizer init");
    // onStart = (d) {};
    onUpdate = (d) {
      _debounceTimer?.cancel();
      if (d.pointerCount == 1 && _currentState != GestureState.oneFingerPan) {
        onOneFingerStartDebounce(d);
      } else if (d.pointerCount == 2 &&
          _currentState != GestureState.twoFingerScale &&
          _currentState != GestureState.twoFingerScroll) {
        if (twoFingerScrollEnabled) {
          _enterTwoFingerPending(d);
        } else {
          onTwoFingerStartDebounce(d);
        }
      } else if (d.pointerCount == 3 &&
          !twoFingerScrollEnabled &&
          _currentState != GestureState.threeFingerVerticalDrag) {
        _currentState = GestureState.threeFingerVerticalDrag;
        if (onThreeFingerVerticalDragStart != null) {
          onThreeFingerVerticalDragStart!(
              DragStartDetails(globalPosition: d.localFocalPoint));
        }
        debugPrint("start threeFingerScale");
      }
      if (_currentState != GestureState.none) {
        switch (_currentState) {
          case GestureState.oneFingerPan:
            if (onOneFingerPanUpdate != null) {
              onOneFingerPanUpdate!(_getDragUpdateDetails(d));
            }
            break;
          case GestureState.twoFingerPending:
            _discriminateTwoFinger(d);
            break;
          case GestureState.twoFingerScroll:
            if (onTwoFingerScrollUpdate != null) {
              onTwoFingerScrollUpdate!(_getDragUpdateDetails(d));
            }
            break;
          case GestureState.twoFingerScale:
            if (onTwoFingerScaleUpdate != null) {
              onTwoFingerScaleUpdate!(d);
            }
            break;
          case GestureState.threeFingerVerticalDrag:
            if (onThreeFingerVerticalDragUpdate != null) {
              onThreeFingerVerticalDragUpdate!(_getDragUpdateDetails(d));
            }
            break;
          default:
            break;
        }
        return;
      }
    };
    onEnd = (d) {
      debugPrint("ScaleGestureRecognizer onEnd");
      _debounceTimer?.cancel();
      // end
      switch (_currentState) {
        case GestureState.oneFingerPan:
          debugPrint("OneFingerState.pan onEnd");
          if (onOneFingerPanEnd != null) {
            onOneFingerPanEnd!(_getDragEndDetails(d));
          }
          break;
        case GestureState.twoFingerPending:
          // Never committed to either scale or scroll; just reset.
          debugPrint("TwoFingerState.pending onEnd");
          break;
        case GestureState.twoFingerScroll:
          debugPrint("TwoFingerState.scroll onEnd");
          if (onTwoFingerScrollEnd != null) {
            onTwoFingerScrollEnd!(_getDragEndDetails(d));
          }
          break;
        case GestureState.twoFingerScale:
          debugPrint("TwoFingerState.scale onEnd");
          if (onTwoFingerScaleEnd != null) {
            onTwoFingerScaleEnd!(d);
          }
          if (isSpecialHoldDragActive) {
            // If we are in special drag mode, we need to reset the state.
            // Otherwise, the next `onTwoFingerScaleUpdate()` will handle a wrong `focalPoint`.
            _currentState = GestureState.none;
            return;
          }
          break;
        case GestureState.threeFingerVerticalDrag:
          debugPrint("ThreeFingerState.vertical onEnd");
          if (onThreeFingerVerticalDragEnd != null) {
            onThreeFingerVerticalDragEnd!(_getDragEndDetails(d));
          }
          break;
        default:
          break;
      }
      _debounceTimer = Timer(Duration(milliseconds: 200), () {
        _currentState = GestureState.none;
      });
    };
  }

  // FIXME: This debounce logic is not working properly.
  // If we move our finger very fast, we won't be able to detect the "oneFingerPan" event sometimes.
  void onOneFingerStartDebounce(ScaleUpdateDetails d) {
    start(ScaleUpdateDetails d) {
      _currentState = GestureState.oneFingerPan;
      if (onOneFingerPanStart != null) {
        onOneFingerPanStart!(DragStartDetails(
            localPosition: d.localFocalPoint, globalPosition: d.focalPoint));
      }
    }

    if (_currentState != GestureState.none) {
      _debounceTimer = Timer(Duration(milliseconds: 200), () {
        start(d);
        debugPrint("debounce start oneFingerPan");
      });
    } else {
      start(d);
      debugPrint("start oneFingerPan");
    }
  }

  void onTwoFingerStartDebounce(ScaleUpdateDetails d) {
    start(ScaleUpdateDetails d) {
      _currentState = GestureState.twoFingerScale;
      if (onTwoFingerScaleStart != null) {
        onTwoFingerScaleStart!(ScaleStartDetails(
            localFocalPoint: d.localFocalPoint, focalPoint: d.focalPoint));
      }
    }

    if (_currentState == GestureState.threeFingerVerticalDrag) {
      _debounceTimer = Timer(Duration(milliseconds: 200), () {
        start(d);
        debugPrint("debounce start twoFingerScale");
      });
    } else {
      start(d);
      debugPrint("start twoFingerScale");
    }
  }

  /// Enters the two-finger pending state and records the initial span ratio so
  /// subsequent updates can be discriminated between pinch-zoom and vertical
  /// scroll.
  void _enterTwoFingerPending(ScaleUpdateDetails d) {
    if (_currentState == GestureState.twoFingerPending) return;
    _currentState = GestureState.twoFingerPending;
    _twoFingerStartScale = d.scale;
    _twoFingerCumDx = 0.0;
    _twoFingerCumDy = 0.0;
    debugPrint("start twoFingerPending");
  }

  /// Discriminates a pending two-finger gesture.
  ///
  /// Order of checks:
  /// 1. If the span ratio deviates from the initial one beyond
  ///    [kTwoFingerScaleThreshold], the gesture is committed to
  ///    [GestureState.twoFingerScale] (pinch zoom keeps working exactly as
  ///    before).
  /// 2. Otherwise, once the cumulative focal-point movement is vertically
  ///    dominant ([kTwoFingerDirectionRatio]) and exceeds
  ///    [kTwoFingerScrollThreshold], the gesture is committed to
  ///    [GestureState.twoFingerScroll] (remote scroll).
  /// 3. If it is horizontally dominant instead, the gesture is committed to
  ///    [GestureState.twoFingerScale] so the canvas pans horizontally.
  /// 4. A drag that satisfies neither dominance test is committed to
  ///    [GestureState.twoFingerScale] once its total travel exceeds
  ///    [kTwoFingerPanFallbackThreshold], so no two-finger drag is a no-op.
  void _discriminateTwoFinger(ScaleUpdateDetails d) {
    final scaleDelta = (d.scale - _twoFingerStartScale).abs();
    _twoFingerCumDx += d.focalPointDelta.dx;
    _twoFingerCumDy += d.focalPointDelta.dy;

    // 1. Span changed -> pinch zoom.
    if (scaleDelta > kTwoFingerScaleThreshold) {
      _commitTwoFingerScale(d);
      return;
    }

    // 2. Vertically dominant -> remote scroll. Kept exactly as before.
    if (_twoFingerCumDy.abs() > kTwoFingerScrollThreshold &&
        _twoFingerCumDy.abs() >
            _twoFingerCumDx.abs() * kTwoFingerDirectionRatio) {
      _currentState = GestureState.twoFingerScroll;
      if (onTwoFingerScrollStart != null) {
        onTwoFingerScrollStart!(DragStartDetails(
            localPosition: d.localFocalPoint, globalPosition: d.focalPoint));
      }
      debugPrint("twoFingerPending -> twoFingerScroll");
      return;
    }

    // 3. Horizontally dominant -> local pan. A parallel two-finger drag keeps
    //    the span constant, so test 1 never fires and test 2 never fires
    //    either: without this branch the gesture stayed pending forever and
    //    panX/panY were never applied, i.e. the canvas could not be moved
    //    after zooming in.
    if (_twoFingerCumDx.abs() > kTwoFingerScrollThreshold &&
        _twoFingerCumDx.abs() >
            _twoFingerCumDy.abs() * kTwoFingerDirectionRatio) {
      _commitTwoFingerScale(d);
      return;
    }

    // 4. Slow or diagonal drag that never clearly matched either axis: commit
    //    to pan instead of swallowing the gesture.
    if (_twoFingerCumDx.abs() + _twoFingerCumDy.abs() >
        kTwoFingerPanFallbackThreshold) {
      _commitTwoFingerScale(d);
    }
  }

  /// Commits a pending two-finger gesture to [GestureState.twoFingerScale].
  ///
  /// That is the only state in which the canvas pan callbacks (`panX`/`panY`)
  /// are invoked, so both horizontal dragging and the undecided fallback go
  /// through here.
  void _commitTwoFingerScale(ScaleUpdateDetails d) {
    _currentState = GestureState.twoFingerScale;
    if (onTwoFingerScaleStart != null) {
      onTwoFingerScaleStart!(ScaleStartDetails(
          localFocalPoint: d.localFocalPoint, focalPoint: d.focalPoint));
    }
    debugPrint("twoFingerPending -> twoFingerScale");
  }

  DragUpdateDetails _getDragUpdateDetails(ScaleUpdateDetails d) =>
      DragUpdateDetails(
          globalPosition: d.focalPoint,
          localPosition: d.localFocalPoint,
          delta: d.focalPointDelta);

  DragEndDetails _getDragEndDetails(ScaleEndDetails d) =>
      DragEndDetails(velocity: d.velocity);

  @override
  void rejectGesture(int pointer) {
    super.rejectGesture(pointer);
    switch (_currentState) {
      case GestureState.oneFingerPan:
        if (onOneFingerPanCancel != null) {
          onOneFingerPanCancel!();
        }
        break;
      case GestureState.twoFingerPending:
      case GestureState.twoFingerScroll:
        // Reset scroll state if needed, currently self-contained
        break;
      case GestureState.twoFingerScale:
        // Reset scale state if needed, currently self-contained
        break;
      case GestureState.threeFingerVerticalDrag:
        // Reset drag state if needed, currently self-contained
        break;
      default:
        break;
    }
    _currentState = GestureState.none;
  }
}

class HoldTapMoveGestureRecognizer extends GestureRecognizer {
  HoldTapMoveGestureRecognizer({
    Object? debugOwner,
    Set<PointerDeviceKind>? supportedDevices,
  }) : super(
          debugOwner: debugOwner,
          supportedDevices: supportedDevices,
        );

  GestureDragStartCallback? onHoldDragStart;
  GestureDragUpdateCallback? onHoldDragUpdate;
  GestureDragDownCallback? onHoldDragDown;
  GestureDragCancelCallback? onHoldDragCancel;
  GestureDragEndCallback? onHoldDragEnd;

  bool _isStart = false;

  Timer? _firstTapUpTimer;
  Timer? _secondTapDownTimer;
  _TapTracker? _firstTap;
  _TapTracker? _secondTap;

  PointerDownEvent? _lastPointerDownEvent;

  final Map<int, _TapTracker> _trackers = <int, _TapTracker>{};

  @override
  bool isPointerAllowed(PointerDownEvent event) {
    if (_firstTap == null) {
      switch (event.buttons) {
        case kPrimaryButton:
          if (onHoldDragStart == null &&
              onHoldDragUpdate == null &&
              onHoldDragCancel == null &&
              onHoldDragEnd == null) {
            return false;
          }
          break;
        default:
          return false;
      }
    }
    return super.isPointerAllowed(event);
  }

  @override
  void addAllowedPointer(PointerDownEvent event) {
    if (_firstTap != null) {
      if (!_firstTap!.isWithinGlobalTolerance(event, kDoubleTapSlop)) {
        // Ignore out-of-bounds second taps.
        return;
      } else if (!_firstTap!.hasElapsedMinTime() ||
          !_firstTap!.hasSameButton(event)) {
        // Restart when the second tap is too close to the first (touch screens
        // often detect touches intermittently), or when buttons mismatch.
        _reset();
        return _trackTap(event);
      } else if (onHoldDragDown != null) {
        invokeCallback<void>(
            'onHoldDragDown',
            () => onHoldDragDown!(DragDownDetails(
                globalPosition: event.position,
                localPosition: event.localPosition)));
      }
    }
    _trackTap(event);
  }

  void _trackTap(PointerDownEvent event) {
    _stopFirstTapUpTimer();
    _stopSecondTapDownTimer();
    final _TapTracker tracker = _TapTracker(
      event: event,
      entry: GestureBinding.instance.gestureArena.add(event.pointer, this),
      doubleTapMinTime: kDoubleTapMinTime,
      gestureSettings: gestureSettings,
    );
    _trackers[event.pointer] = tracker;
    _lastPointerDownEvent = event;
    tracker.startTrackingPointer(_handleEvent, event.transform);
  }

  void _handleEvent(PointerEvent event) {
    final _TapTracker tracker = _trackers[event.pointer]!;
    if (event is PointerUpEvent) {
      if (_firstTap == null && _secondTap == null) {
        _registerFirstTap(tracker);
      } else if (_secondTap != null) {
        if (event.pointer == _secondTap!.pointer) {
          if (onHoldDragEnd != null) {
            onHoldDragEnd!(DragEndDetails());
            _secondTap = null;
            _isStart = false;
          }
        }
      } else {
        _reject(tracker);
      }
    } else if (event is PointerDownEvent) {
      if (_firstTap != null && _secondTap == null) {
        _registerSecondTap(tracker);
      }
    } else if (event is PointerMoveEvent) {
      if (!tracker.isWithinGlobalTolerance(event, kDoubleTapTouchSlop)) {
        if (_firstTap != null && _firstTap!.pointer == event.pointer) {
          // first tap move
          _reject(tracker);
        } else if (_secondTap != null && _secondTap!.pointer == event.pointer) {
          // debugPrint("_secondTap move");
          // second tap move
          if (!_isStart) {
            _resolve();
          }
          if (onHoldDragUpdate != null) {
            onHoldDragUpdate!(DragUpdateDetails(
                globalPosition: event.position,
                localPosition: event.localPosition,
                delta: event.delta));
          }
        }
      }
    } else if (event is PointerCancelEvent) {
      _reject(tracker);
    }
  }

  @override
  void acceptGesture(int pointer) {}

  @override
  void rejectGesture(int pointer) {
    _TapTracker? tracker = _trackers[pointer];
    // If tracker isn't in the list, check if this is the first tap tracker
    if (tracker == null && _firstTap != null && _firstTap!.pointer == pointer) {
      tracker = _firstTap;
    }
    // If tracker is still null, we rejected ourselves already
    if (tracker != null) {
      _reject(tracker);
    }
  }

  void _resolve() {
    _stopSecondTapDownTimer();
    _firstTap?.entry.resolve(GestureDisposition.accepted);
    _secondTap?.entry.resolve(GestureDisposition.accepted);
    _isStart = true;
    // TODO start details
    if (onHoldDragStart != null) {
      onHoldDragStart!(DragStartDetails(
        kind: _lastPointerDownEvent?.kind,
      ));
    }
  }

  void _reject(_TapTracker tracker) {
    try {
      _checkCancel();
      _isStart = false;
      _trackers.remove(tracker.pointer);
      tracker.entry.resolve(GestureDisposition.rejected);
      _freezeTracker(tracker);
      _reset();
    } catch (e) {
      debugPrint("Failed to _reject:$e");
    }
  }

  @override
  void dispose() {
    _reset();
    super.dispose();
  }

  void _reset() {
    _isStart = false;
    // debugPrint("reset");
    _stopFirstTapUpTimer();
    _stopSecondTapDownTimer();
    if (_firstTap != null) {
      if (_trackers.isNotEmpty) {
        _checkCancel();
      }
      // Note, order is important below in order for the resolve -> reject logic
      // to work properly.
      final _TapTracker tracker = _firstTap!;
      _firstTap = null;
      _reject(tracker);
      GestureBinding.instance.gestureArena.release(tracker.pointer);

      if (_secondTap != null) {
        final _TapTracker tracker = _secondTap!;
        _secondTap = null;
        _reject(tracker);
        GestureBinding.instance.gestureArena.release(tracker.pointer);
      }
    }
    _firstTap = null;
    _secondTap = null;
    _clearTrackers();
  }

  void _registerFirstTap(_TapTracker tracker) {
    _startFirstTapUpTimer();
    GestureBinding.instance.gestureArena.hold(tracker.pointer);
    // Note, order is important below in order for the clear -> reject logic to
    // work properly.
    _freezeTracker(tracker);
    _trackers.remove(tracker.pointer);
    _firstTap = tracker;
  }

  void _registerSecondTap(_TapTracker tracker) {
    if (_firstTap != null) {
      _stopFirstTapUpTimer();
      _freezeTracker(_firstTap!);
      _firstTap = null;
    }

    _startSecondTapDownTimer();
    GestureBinding.instance.gestureArena.hold(tracker.pointer);

    _secondTap = tracker;

    // TODO
  }

  void _clearTrackers() {
    _trackers.values.toList().forEach(_reject);
    assert(_trackers.isEmpty);
  }

  void _freezeTracker(_TapTracker tracker) {
    tracker.stopTrackingPointer(_handleEvent);
  }

  void _startFirstTapUpTimer() {
    _firstTapUpTimer ??= Timer(kDoubleTapTimeout, _reset);
  }

  void _startSecondTapDownTimer() {
    _secondTapDownTimer ??= Timer(kDoubleTapTimeout, _resolve);
  }

  void _stopFirstTapUpTimer() {
    if (_firstTapUpTimer != null) {
      _firstTapUpTimer!.cancel();
      _firstTapUpTimer = null;
    }
  }

  void _stopSecondTapDownTimer() {
    if (_secondTapDownTimer != null) {
      _secondTapDownTimer!.cancel();
      _secondTapDownTimer = null;
    }
  }

  void _checkCancel() {
    if (onHoldDragCancel != null) {
      invokeCallback<void>('onHoldDragCancel', onHoldDragCancel!);
    }
  }

  @override
  String get debugDescription => 'double tap';
}

class DoubleFinerTapGestureRecognizer extends GestureRecognizer {
  DoubleFinerTapGestureRecognizer({
    Object? debugOwner,
    Set<PointerDeviceKind>? supportedDevices,
  }) : super(
          debugOwner: debugOwner,
          supportedDevices: supportedDevices,
        );

  GestureTapDownCallback? onDoubleFinerTapDown;
  GestureTapDownCallback? onDoubleFinerTap;
  GestureTapCancelCallback? onDoubleFinerTapCancel;

  Timer? _firstTapTimer;
  _TapTracker? _firstTap;

  PointerDownEvent? _lastPointerDownEvent;

  var _isStart = false;

  final Set<int> _upTap = {};

  final Map<int, _TapTracker> _trackers = <int, _TapTracker>{};

  @override
  bool isPointerAllowed(PointerDownEvent event) {
    if (_firstTap == null) {
      switch (event.buttons) {
        case kPrimaryButton:
          if (onDoubleFinerTapDown == null &&
              onDoubleFinerTap == null &&
              onDoubleFinerTapCancel == null) {
            return false;
          }
          break;
        default:
          return false;
      }
    }
    return super.isPointerAllowed(event);
  }

  @override
  void addAllowedPointer(PointerDownEvent event) {
    debugPrint("addAllowedPointer");
    if (_isStart) {
      // second
      if (onDoubleFinerTapDown != null) {
        final TapDownDetails details = TapDownDetails(
          globalPosition: event.position,
          localPosition: event.localPosition,
          kind: getKindForPointer(event.pointer),
        );
        invokeCallback<void>(
            'onDoubleFinerTapDown', () => onDoubleFinerTapDown!(details));
      }
    } else {
      // first tap
      _isStart = true;
      _lastPointerDownEvent = event;
      _startFirstTapDownTimer();
    }
    _trackTap(event);
  }

  void _trackTap(PointerDownEvent event) {
    final _TapTracker tracker = _TapTracker(
      event: event,
      entry: GestureBinding.instance.gestureArena.add(event.pointer, this),
      doubleTapMinTime: kDoubleTapMinTime,
      gestureSettings: gestureSettings,
    );
    _trackers[event.pointer] = tracker;
    // debugPrint("_trackers:$_trackers");
    tracker.startTrackingPointer(_handleEvent, event.transform);

    _registerTap(tracker);
  }

  void _handleEvent(PointerEvent event) {
    final _TapTracker tracker = _trackers[event.pointer]!;
    if (event is PointerUpEvent) {
      debugPrint("PointerUpEvent");
      _upTap.add(tracker.pointer);
    } else if (event is PointerMoveEvent) {
      if (!tracker.isWithinGlobalTolerance(event, kDoubleTapTouchSlop)) {
        _reject(tracker);
      }
    } else if (event is PointerCancelEvent) {
      _reject(tracker);
    }
  }

  @override
  void acceptGesture(int pointer) {}

  @override
  void rejectGesture(int pointer) {
    _TapTracker? tracker = _trackers[pointer];
    // If tracker isn't in the list, check if this is the first tap tracker
    if (tracker == null && _firstTap != null && _firstTap!.pointer == pointer) {
      tracker = _firstTap;
    }
    // If tracker is still null, we rejected ourselves already
    if (tracker != null) {
      _reject(tracker);
    }
  }

  void _reject(_TapTracker tracker) {
    _trackers.remove(tracker.pointer);
    tracker.entry.resolve(GestureDisposition.rejected);
    _freezeTracker(tracker);
    if (_firstTap != null) {
      if (tracker == _firstTap) {
        _reset();
      } else {
        _checkCancel();
        if (_trackers.isEmpty) {
          _reset();
        }
      }
    }
  }

  @override
  void dispose() {
    _reset();
    super.dispose();
  }

  void _reset() {
    _stopFirstTapUpTimer();
    _firstTap = null;
    _clearTrackers();
  }

  void _registerTap(_TapTracker tracker) {
    GestureBinding.instance.gestureArena.hold(tracker.pointer);
    // Note, order is important below in order for the clear -> reject logic to
    // work properly.
  }

  void _clearTrackers() {
    _trackers.values.toList().forEach(_reject);
    assert(_trackers.isEmpty);
  }

  void _freezeTracker(_TapTracker tracker) {
    tracker.stopTrackingPointer(_handleEvent);
  }

  void _startFirstTapDownTimer() {
    _firstTapTimer ??= Timer(kDoubleTapTimeout, _timeoutCheck);
  }

  void _stopFirstTapUpTimer() {
    if (_firstTapTimer != null) {
      _firstTapTimer!.cancel();
      _firstTapTimer = null;
    }
  }

  void _timeoutCheck() {
    _isStart = false;
    if (_upTap.length == 2) {
      _resolve();
    } else {
      _reset();
    }
    _upTap.clear();
  }

  void _resolve() {
    // TODO tap down details
    if (onDoubleFinerTap != null) {
      onDoubleFinerTap!(TapDownDetails(
        kind: _lastPointerDownEvent?.kind,
      ));
    }
    _trackers.forEach((key, value) {
      value.entry.resolve(GestureDisposition.accepted);
    });
    _reset();
  }

  void _checkCancel() {
    if (onDoubleFinerTapCancel != null) {
      invokeCallback<void>('onHoldDragCancel', onDoubleFinerTapCancel!);
    }
  }

  @override
  String get debugDescription => 'double tap';
}

/// TapTracker helps track individual tap sequences as part of a
/// larger gesture.
class _TapTracker {
  _TapTracker({
    required PointerDownEvent event,
    required this.entry,
    required Duration doubleTapMinTime,
    required this.gestureSettings,
  })  : pointer = event.pointer,
        _initialGlobalPosition = event.position,
        initialButtons = event.buttons,
        _doubleTapMinTimeCountdown =
            _CountdownZoned(duration: doubleTapMinTime);

  final DeviceGestureSettings? gestureSettings;
  final int pointer;
  final GestureArenaEntry entry;
  final Offset _initialGlobalPosition;
  final int initialButtons;
  final _CountdownZoned _doubleTapMinTimeCountdown;

  bool _isTrackingPointer = false;

  void startTrackingPointer(PointerRoute route, Matrix4? transform) {
    if (!_isTrackingPointer) {
      _isTrackingPointer = true;
      GestureBinding.instance.pointerRouter.addRoute(pointer, route, transform);
    }
  }

  void stopTrackingPointer(PointerRoute route) {
    if (_isTrackingPointer) {
      _isTrackingPointer = false;
      GestureBinding.instance.pointerRouter.removeRoute(pointer, route);
    }
  }

  bool isWithinGlobalTolerance(PointerEvent event, double tolerance) {
    final Offset offset = event.position - _initialGlobalPosition;
    return offset.distance <= tolerance;
  }

  bool hasElapsedMinTime() {
    return _doubleTapMinTimeCountdown.timeout;
  }

  bool hasSameButton(PointerDownEvent event) {
    return event.buttons == initialButtons;
  }
}

/// CountdownZoned tracks whether the specified duration has elapsed since
/// creation, honoring [Zone].
class _CountdownZoned {
  _CountdownZoned({required Duration duration}) {
    Timer(duration, _onTimeout);
  }

  bool _timeout = false;

  bool get timeout => _timeout;

  void _onTimeout() {
    _timeout = true;
  }
}

RawGestureDetector getMixinGestureDetector({
  Widget? child,
  GestureTapUpCallback? onTapUp,
  GestureTapDownCallback? onDoubleTapDown,
  GestureDoubleTapCallback? onDoubleTap,
  GestureLongPressDownCallback? onLongPressDown,
  GestureLongPressCallback? onLongPress,
  GestureDragStartCallback? onHoldDragStart,
  GestureDragUpdateCallback? onHoldDragUpdate,
  GestureDragCancelCallback? onHoldDragCancel,
  GestureDragEndCallback? onHoldDragEnd,
  GestureTapDownCallback? onDoubleFinerTap,
  GestureDragStartCallback? onOneFingerPanStart,
  GestureDragUpdateCallback? onOneFingerPanUpdate,
  GestureDragEndCallback? onOneFingerPanEnd,
  GestureDragCancelCallback? onOneFingerPanCancel,
  GestureScaleUpdateCallback? onTwoFingerScaleUpdate,
  GestureScaleEndCallback? onTwoFingerScaleEnd,
  GestureDragUpdateCallback? onThreeFingerVerticalDragUpdate,
}) {
  return RawGestureDetector(
      child: child,
      gestures: <Type, GestureRecognizerFactory>{
        // Official
        TapGestureRecognizer:
            GestureRecognizerFactoryWithHandlers<TapGestureRecognizer>(
                () => TapGestureRecognizer(), (instance) {
          instance.onTapUp = onTapUp;
        }),
        DoubleTapGestureRecognizer:
            GestureRecognizerFactoryWithHandlers<DoubleTapGestureRecognizer>(
                () => DoubleTapGestureRecognizer(), (instance) {
          instance
            ..onDoubleTapDown = onDoubleTapDown
            ..onDoubleTap = onDoubleTap;
        }),
        LongPressGestureRecognizer:
            GestureRecognizerFactoryWithHandlers<LongPressGestureRecognizer>(
                () => LongPressGestureRecognizer(), (instance) {
          instance
            ..onLongPressDown = onLongPressDown
            ..onLongPress = onLongPress;
        }),
        // Customized
        HoldTapMoveGestureRecognizer:
            GestureRecognizerFactoryWithHandlers<HoldTapMoveGestureRecognizer>(
                () => HoldTapMoveGestureRecognizer(),
                (instance) => instance
                  ..onHoldDragStart = onHoldDragStart
                  ..onHoldDragUpdate = onHoldDragUpdate
                  ..onHoldDragCancel = onHoldDragCancel
                  ..onHoldDragEnd = onHoldDragEnd),
        DoubleFinerTapGestureRecognizer: GestureRecognizerFactoryWithHandlers<
                DoubleFinerTapGestureRecognizer>(
            () => DoubleFinerTapGestureRecognizer(), (instance) {
          instance.onDoubleFinerTap = onDoubleFinerTap;
        }),
        CustomTouchGestureRecognizer:
            GestureRecognizerFactoryWithHandlers<CustomTouchGestureRecognizer>(
                () => CustomTouchGestureRecognizer(), (instance) {
          instance
            ..onOneFingerPanStart = onOneFingerPanStart
            ..onOneFingerPanUpdate = onOneFingerPanUpdate
            ..onOneFingerPanEnd = onOneFingerPanEnd
            ..onOneFingerPanCancel = onOneFingerPanCancel
            ..onTwoFingerScaleUpdate = onTwoFingerScaleUpdate
            ..onTwoFingerScaleEnd = onTwoFingerScaleEnd
            ..onThreeFingerVerticalDragUpdate = onThreeFingerVerticalDragUpdate;
        }),
      });
}
