/*
 * Copyright 2022 The Blaze Authors
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

package org.apache.spark.sql.blaze.execution

import java.io.EOFException
import java.io.InputStream
import java.nio.channels.Channels
import java.nio.ByteBuffer
import java.nio.ByteOrder

import org.apache.spark.internal.Logging

case class IpcInputStreamIterator(in: InputStream) extends Iterator[IpcData] with Logging {
  private val channel = Channels.newChannel(in)
  private val ipcLengthsBuf = ByteBuffer.allocate(16).order(ByteOrder.LITTLE_ENDIAN)

  // NOTE:
  // since all ipcs are sharing the same input stream and channel, the second
  // hasNext() must be called after the first ipc has been completely processed.

  private var consumed = true
  private var finished = false
  private var currentIpcLength = 0L
  private var currentIpcLengthUncompressed = 0L

  override def hasNext: Boolean = {
    !finished && {
      if (!consumed) {
        return true
      }
      ipcLengthsBuf.clear()
      while (ipcLengthsBuf.hasRemaining && channel.read(ipcLengthsBuf) >= 0) {}

      if (ipcLengthsBuf.hasRemaining) {
        if (ipcLengthsBuf.position() == 0) {
          finished = true
          return false
        }
        throw new EOFException(
          "Data corrupt: unexpected EOF while reading compressed ipc lengths")
      }
      ipcLengthsBuf.flip()
      currentIpcLength = ipcLengthsBuf.getLong
      currentIpcLengthUncompressed = ipcLengthsBuf.getLong
      consumed = false
      return true
    }
  }

  override def next(): IpcData = {
    consumed = true
    IpcData(channel, compressed = true, currentIpcLength, currentIpcLengthUncompressed)
  }
}
