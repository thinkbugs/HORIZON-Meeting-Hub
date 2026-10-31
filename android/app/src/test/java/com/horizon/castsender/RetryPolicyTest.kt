package com.horizon.castsender

import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test

/** The sender must reconnect quickly, then give up before it nags forever. */
class RetryPolicyTest {
    @Test
    fun backoffDoublesUpToTheCap() {
        assertEquals(1_000L, MirrorService.retryDelayMs(1))
        assertEquals(2_000L, MirrorService.retryDelayMs(2))
        assertEquals(4_000L, MirrorService.retryDelayMs(3))
        assertEquals(8_000L, MirrorService.retryDelayMs(4))
        assertEquals(8_000L, MirrorService.retryDelayMs(MirrorService.MAX_RETRIES))
    }

    @Test
    fun totalPatienceStaysUnderHalfAMinute() {
        val total = (1..MirrorService.MAX_RETRIES).sumOf { MirrorService.retryDelayMs(it) }
        assertTrue("$total ms is too long to hold a dead session", total <= 30_000L)
    }
}
