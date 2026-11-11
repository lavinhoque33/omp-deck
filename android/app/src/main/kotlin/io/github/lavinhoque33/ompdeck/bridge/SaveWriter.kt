package io.github.lavinhoque33.ompdeck.bridge

import android.content.ContentResolver
import android.net.Uri
import java.io.IOException

/** Writes the document the user picked in the system "save as" dialog (a content Uri we were handed, never a path). */
object SaveWriter {
    /** Replaces the document's content with exactly [bytes]. */
    fun write(resolver: ContentResolver, uri: Uri, bytes: ByteArray) {
        val out = resolver.openOutputStream(uri, "wt") ?: throw IOException("the document cannot be opened for writing")
        out.use { it.write(bytes) }
    }
}
