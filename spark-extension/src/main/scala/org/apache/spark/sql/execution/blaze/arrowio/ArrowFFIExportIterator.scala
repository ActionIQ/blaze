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

package org.apache.spark.sql.execution.blaze.arrowio

import org.apache.arrow.c.ArrowArray
import org.apache.arrow.c.ArrowSchema
import org.apache.arrow.c.Data
import org.apache.arrow.vector.VectorSchemaRoot
import org.apache.arrow.vector.dictionary.DictionaryProvider.MapDictionaryProvider
import org.apache.spark.sql.catalyst.InternalRow
import org.apache.spark.sql.execution.blaze.arrowio.util2.ArrowUtils2
import org.apache.spark.sql.execution.blaze.arrowio.util2.ArrowWriter
import org.apache.spark.sql.types.StructType
import org.apache.spark.TaskContext
import org.apache.spark.internal.Logging

class ArrowFFIExportIterator(
    rowIter: Iterator[InternalRow],
    schema: StructType,
    timeZoneId: String,
    taskContext: TaskContext,
    recordBatchSize: Int = 10000)
    extends Iterator[(Long, Long) => Unit]
    with Logging {

  private val arrowSchema = ArrowUtils2.toArrowSchema(schema, timeZoneId)
  private var allocator =
    ArrowUtils2.rootAllocator.newChildAllocator("arrowFFIExportIterator", 0, Long.MaxValue)
  private var root = VectorSchemaRoot.create(arrowSchema, allocator)
  private val arrowWriter: ArrowWriter = ArrowWriter.create(root)
  private val emptyDictionaryProvider = new MapDictionaryProvider()

  taskContext.addTaskCompletionListener[Unit](_ => close())

  override def hasNext: Boolean = allocator != null && rowIter.hasNext

  override def next(): (Long, Long) => Unit = {
    var rowCount = 0
    arrowWriter.reset()
    root.clear()

    while (rowIter.hasNext && rowCount < recordBatchSize) {
      arrowWriter.write(rowIter.next())
      rowCount += 1
    }
    arrowWriter.finish()

    (exportArrowSchemaPtr: Long, exportArrowArrayPtr: Long) =>
      Data.exportVectorSchemaRoot(
        allocator,
        root,
        emptyDictionaryProvider,
        ArrowArray.wrap(exportArrowArrayPtr),
        ArrowSchema.wrap(exportArrowSchemaPtr))
  }

  private def close(): Unit =
    synchronized {
      if (allocator != null) {
        root.close()
        root = null
        allocator.close()
        allocator = null
      }
    }
}
