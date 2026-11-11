package io.github.lavinhoque33.ompdeck

import android.net.Uri
import androidx.test.platform.app.InstrumentationRegistry
import io.github.lavinhoque33.ompdeck.bridge.BridgeProtocol
import io.github.lavinhoque33.ompdeck.bridge.SaveWriter
import org.junit.Assert.assertArrayEquals
import org.junit.Test
import java.io.File
import java.util.Base64
import java.util.UUID

/** What the user's "save as" document ends up containing: exactly the bridge's decoded bytes, replacing older content. */
class SaveWriterInstrumentedTest {
    @Test
    fun writesExactBytesAndTruncatesWhatWasThere() {
        val resolver = InstrumentationRegistry.getInstrumentation().targetContext.contentResolver
        val file = File(InstrumentationRegistry.getInstrumentation().targetContext.cacheDir, "save-${UUID.randomUUID()}.bin")
        try {
            file.writeBytes(ByteArray(5000) { 7 })
            val original = ByteArray(1500) { (it * 31).toByte() }
            val decoded = BridgeProtocol.decodeSaveData(Base64.getEncoder().encodeToString(original))
            SaveWriter.write(resolver, Uri.fromFile(file), decoded)
            assertArrayEquals(original, file.readBytes())
        } finally {
            file.delete()
        }
    }
}
