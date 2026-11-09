package net.mahirsn.mmspad.input

import android.view.MotionEvent
import net.mahirsn.mmspad.network.InputEvent
import net.mahirsn.mmspad.network.TimestampedInputEvent
import kotlin.math.cos
import kotlin.math.sin

/**
 * Captures stylus and touch input from MotionEvents.
 */
object InputCapture {
    private var lastButtonState = 0
    var isTouchEnabled: Boolean = false

    /** MotionEvent.TOOL_TYPE_PALM, hidden from the SDK. */
    private const val TOOL_TYPE_PALM = 5

    /** Finger ids that are down on the PC. */
    private val fingersDown = mutableSetOf<Int>()

    /**
     * Convert a MotionEvent to TimestampedInputEvents.
     * Each event includes its proper timestamp for accurate retiming on the server.
     *
     * @param event The motion event from the view
     * @param viewWidth Width of the view for normalization
     * @param viewHeight Height of the view for normalization
     * @return List of timestamped input events to send
     */
    fun processMotionEvent(
        event: MotionEvent,
        viewWidth: Int,
        viewHeight: Int
    ): List<TimestampedInputEvent> {
        val events = mutableListOf<TimestampedInputEvent>()

        when (event.getToolType(0)) {
            MotionEvent.TOOL_TYPE_STYLUS, MotionEvent.TOOL_TYPE_ERASER -> {
                events.addAll(processStylusEvent(event, viewWidth, viewHeight))
            }
            MotionEvent.TOOL_TYPE_FINGER -> {
                // Android has a native TOOL_TYPE_PALM (constant value 3), but it's not always reliable.
                // We check strictly for FINGER here, but we can also filter explicitly if needed.
                events.addAll(processTouchEvent(event, viewWidth, viewHeight))
            }
        }

        return events
    }

    private fun processStylusEvent(
        event: MotionEvent,
        viewWidth: Int,
        viewHeight: Int
    ): List<TimestampedInputEvent> {
        val events = mutableListOf<TimestampedInputEvent>()
        
        // Current event timestamp in microseconds (eventTime is in milliseconds)
        val currentTimestamp = event.eventTime * 1000

        // Normalize coordinates to 0-1 range
        val x = event.x / viewWidth
        val y = event.y / viewHeight
        
        // Only report pressure when actually touching the screen
        // During hover, pressure should be 0 to prevent false strokes
        val isHovering = event.actionMasked == MotionEvent.ACTION_HOVER_MOVE ||
                         event.actionMasked == MotionEvent.ACTION_HOVER_ENTER ||
                         event.actionMasked == MotionEvent.ACTION_HOVER_EXIT
        val pressure = if (isHovering) 0f else event.pressure

        // Calculate tilt from orientation and tilt axes
        val tilt = event.getAxisValue(MotionEvent.AXIS_TILT)
        val orientation = event.getAxisValue(MotionEvent.AXIS_ORIENTATION)
        val tiltX = (Math.toDegrees(tilt.toDouble()) * cos(orientation.toDouble())).toFloat()
        val tiltY = (Math.toDegrees(tilt.toDouble()) * sin(orientation.toDouble())).toFloat()

        // Check for button state changes on every event
        val currentButtonState = event.buttonState
        if (currentButtonState != lastButtonState) {
            // Button 0 (Primary / Right Click)
            // Some devices map stylus button to BUTTON_SECONDARY (Right Click)
            val btn0Mask = MotionEvent.BUTTON_STYLUS_PRIMARY or MotionEvent.BUTTON_SECONDARY
            val wasPressed0 = (lastButtonState and btn0Mask) != 0
            val isPressed0 = (currentButtonState and btn0Mask) != 0
            
            if (wasPressed0 != isPressed0) {
                events.add(TimestampedInputEvent(currentTimestamp, InputEvent.StylusButton(0, isPressed0)))
            }

            // Button 1 (Secondary)
            val btn1Mask = MotionEvent.BUTTON_STYLUS_SECONDARY
            val wasPressed1 = (lastButtonState and btn1Mask) != 0
            val isPressed1 = (currentButtonState and btn1Mask) != 0

            if (wasPressed1 != isPressed1) {
                events.add(TimestampedInputEvent(currentTimestamp, InputEvent.StylusButton(1, isPressed1)))
            }

            lastButtonState = currentButtonState
        }

        when (event.actionMasked) {
            MotionEvent.ACTION_DOWN, MotionEvent.ACTION_POINTER_DOWN -> {
                events.add(TimestampedInputEvent(currentTimestamp, InputEvent.StylusDown(x, y, event.pressure, tiltX, tiltY)))
            }
            MotionEvent.ACTION_MOVE -> {
                // Process historical events with their proper timestamps for smooth timing
                for (h in 0 until event.historySize) {
                    val historicalTimestamp = event.getHistoricalEventTime(h) * 1000
                    val hx = event.getHistoricalX(h) / viewWidth
                    val hy = event.getHistoricalY(h) / viewHeight
                    val hp = event.getHistoricalPressure(h)
                    events.add(TimestampedInputEvent(historicalTimestamp, InputEvent.StylusMove(hx, hy, hp, tiltX, tiltY)))
                }
                events.add(TimestampedInputEvent(currentTimestamp, InputEvent.StylusMove(x, y, event.pressure, tiltX, tiltY)))
            }
            MotionEvent.ACTION_HOVER_MOVE, MotionEvent.ACTION_HOVER_ENTER -> {
                // Hover events - pressure is 0
                events.add(TimestampedInputEvent(currentTimestamp, InputEvent.StylusMove(x, y, 0f, tiltX, tiltY)))
            }
            MotionEvent.ACTION_HOVER_EXIT -> {
                events.add(TimestampedInputEvent(currentTimestamp, InputEvent.StylusLeave))
            }
            MotionEvent.ACTION_UP, MotionEvent.ACTION_POINTER_UP, MotionEvent.ACTION_CANCEL -> {
                events.add(TimestampedInputEvent(currentTimestamp, InputEvent.StylusUp))
            }
        }

        return events
    }

    private fun processTouchEvent(
        event: MotionEvent,
        viewWidth: Int,
        viewHeight: Int
    ): List<TimestampedInputEvent> {
        val events = mutableListOf<TimestampedInputEvent>()
        val currentTimestamp = event.eventTime * 1000
        // Lift every finger the PC still has down. A finger whose up is never sent stays
        // on the PC's touchpad, and every later gesture counts one finger more.
        fun liftAll() {
            for (id in fingersDown) events.add(TimestampedInputEvent(currentTimestamp, InputEvent.TouchUp(id)))
            fingersDown.clear()
        }

        // Touch off, or Android marks a contact as a palm: no fingers on the PC.
        if (!isTouchEnabled || (0 until event.pointerCount).any { event.getToolType(it) == TOOL_TYPE_PALM }) {
            liftAll()
            return events
        }
        // A new gesture: whatever was down before has ended.
        if (event.actionMasked == MotionEvent.ACTION_DOWN) liftAll()

        when (event.actionMasked) {
            MotionEvent.ACTION_DOWN, MotionEvent.ACTION_POINTER_DOWN -> {
                val index = event.actionIndex
                val id = event.getPointerId(index)
                // Only fingers: a pen in the same gesture is not a touchpad contact.
                if (event.getToolType(index) != MotionEvent.TOOL_TYPE_FINGER) return events
                val x = event.getX(index) / viewWidth
                val y = event.getY(index) / viewHeight
                events.add(TimestampedInputEvent(currentTimestamp, InputEvent.TouchDown(id, x, y)))
                fingersDown.add(id)
            }
            MotionEvent.ACTION_MOVE -> {
                // Process historical events for smoother curves
                for (h in 0 until event.historySize) {
                    val historicalTimestamp = event.getHistoricalEventTime(h) * 1000
                    for (i in 0 until event.pointerCount) {
                        val id = event.getPointerId(i)
                        if (id !in fingersDown) continue
                        val x = event.getHistoricalX(i, h) / viewWidth
                        val y = event.getHistoricalY(i, h) / viewHeight
                        events.add(TimestampedInputEvent(historicalTimestamp, InputEvent.TouchMove(id, x, y)))
                    }
                }

                // Current event
                for (i in 0 until event.pointerCount) {
                    val id = event.getPointerId(i)
                    if (id !in fingersDown) continue
                    val x = event.getX(i) / viewWidth
                    val y = event.getY(i) / viewHeight
                    events.add(TimestampedInputEvent(currentTimestamp, InputEvent.TouchMove(id, x, y)))
                }
            }
            MotionEvent.ACTION_UP, MotionEvent.ACTION_POINTER_UP -> {
                val id = event.getPointerId(event.actionIndex)
                if (fingersDown.remove(id)) events.add(TimestampedInputEvent(currentTimestamp, InputEvent.TouchUp(id)))
            }
            MotionEvent.ACTION_CANCEL -> liftAll()
        }

        return events
    }
}
