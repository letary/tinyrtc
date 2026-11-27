package dev.tinyrtc

import java.util.concurrent.CountDownLatch
import java.util.concurrent.Executors
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicInteger
import kotlin.random.Random
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineStart
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.async
import kotlinx.coroutines.asCoroutineDispatcher
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import org.junit.After
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * A cancelled caller of a call that makes something on the lane leaves nothing behind: what was
 * made is returned or undone, never dropped (withContext alone drops it: its prompt cancellation
 * throws even when the block has run). The lane is a real single thread, as Core.lane is.
 * CameraPipeline.start (the core's video source) and RtcConnection.createDataChannel go through
 * [madeOn]; the camera's own part of start (Camera2, a HandlerThread) cannot run here.
 */
class MadeOnTest {
    private val executor = Executors.newSingleThreadExecutor()
    private val lane = executor.asCoroutineDispatcher()

    @After
    fun stopLane() {
        executor.shutdownNow()
    }

    @Test
    fun whatWasMadeIsReturned() = runBlocking {
        val undone = ArrayList<Int>()
        val made: Int = madeOn(lane, { 7 }) { undone.add(it) }
        assertEquals(7, made)
        assertTrue(undone.isEmpty())
    }

    @Test
    fun aCallerCancelledWhileItIsMadeHasItUndone() = runBlocking {
        val started = CountDownLatch(1)
        val finish = CountDownLatch(1)
        val undone = ArrayList<Int>()
        val call = async(start = CoroutineStart.UNDISPATCHED) {
            madeOn(lane, {
                started.countDown()
                finish.await()
                42
            }) { undone.add(it) }
        }
        assertTrue(started.await(5, TimeUnit.SECONDS))
        call.cancel()
        finish.countDown() // the block returns 42 to a caller that is already cancelled
        assertTrue(runCatching { call.await() }.exceptionOrNull() is CancellationException)
        assertEquals(listOf(42), undone)
    }

    @Test
    fun aCallerCancelledBeforeTheLaneTookItLeavesNothing() = runBlocking {
        val busy = CountDownLatch(1)
        executor.execute { busy.await() }
        val made = AtomicInteger()
        val undone = AtomicInteger()
        val call = async(start = CoroutineStart.UNDISPATCHED) {
            madeOn(lane, { made.incrementAndGet() }) { undone.incrementAndGet() }
        }
        call.cancel() // it waits for the lane
        busy.countDown()
        assertTrue(runCatching { call.await() }.exceptionOrNull() is CancellationException)
        executor.submit {}.get(5, TimeUnit.SECONDS) // the lane ran whatever it had
        assertEquals(made.get(), undone.get())
    }

    @Test
    fun aFailureLeavesNothingToUndo() = runBlocking {
        val undone = ArrayList<Int>()
        val failure = runCatching { madeOn<Int>(lane, { throw RtcError.Failed("no") }) { undone.add(it) } }.exceptionOrNull()
        assertTrue(failure is RtcError.Failed)
        assertTrue(undone.isEmpty())
    }

    /** Cancelled at any moment — before, while or after the lane made it: made = returned + undone. */
    @Test
    fun whateverTheMomentNothingIsLost() = runBlocking {
        val made = AtomicInteger()
        val returned = AtomicInteger()
        val undone = AtomicInteger()
        val random = Random(1)
        repeat(3000) {
            val work = random.nextLong(0, 30_000)
            val call = launch(Dispatchers.Default) {
                madeOn(lane, {
                    spin(work)
                    made.incrementAndGet()
                }) { undone.incrementAndGet() }
                returned.incrementAndGet()
            }
            spin(random.nextLong(0, 60_000))
            call.cancel()
            call.join()
        }
        executor.submit {}.get(5, TimeUnit.SECONDS)
        assertTrue(made.get() > 0)
        assertEquals(made.get(), returned.get() + undone.get())
    }

    private fun spin(nanos: Long) {
        val until = System.nanoTime() + nanos
        while (System.nanoTime() < until) continue
    }
}
