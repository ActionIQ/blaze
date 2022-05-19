/*
 * Licensed to the Apache Software Foundation (ASF) under one or more
 * contributor license agreements.  See the NOTICE file distributed with
 * this work for additional information regarding copyright ownership.
 * The ASF licenses this file to You under the Apache License, Version 2.0
 * (the "License"); you may not use this file except in compliance with
 * the License.  You may obtain a copy of the License at
 *
 *    http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

package org.apache.spark.sql.blaze

import java.util.concurrent.SynchronousQueue
import java.util.concurrent.LinkedTransferQueue
import java.util.concurrent.TimeUnit

import scala.collection.JavaConverters._
import scala.concurrent.Await
import scala.concurrent.Promise
import scala.concurrent.duration.Duration

import org.apache.arrow.c.ArrowArray
import org.apache.arrow.c.ArrowSchema
import org.apache.arrow.c.CDataDictionaryProvider
import org.apache.arrow.c.Data
import org.apache.arrow.vector.VectorSchemaRoot
import org.apache.spark.TaskContext
import org.apache.spark.sql.catalyst.InternalRow
import org.apache.spark.sql.util2.ArrowColumnVector
import org.apache.spark.sql.util2.ArrowUtils2
import org.apache.spark.sql.vectorized.ColumnarBatch
import org.apache.spark.sql.vectorized.ColumnVector
import org.apache.spark.util.CompletionIterator

object FFIHelper {
  def tryWithResource[R <: AutoCloseable, T](createResource: => R)(f: R => T): T = {
    val resource = createResource
    try f.apply(resource)
    finally resource.close()
  }

  def rootAsBatch(root: VectorSchemaRoot): ColumnarBatch = {
    val columns = root.getFieldVectors.asScala.map { vector =>
      new ArrowColumnVector(vector).asInstanceOf[ColumnVector]
    }.toArray
    val batch = new ColumnarBatch(columns)
    batch.setNumRows(root.getRowCount)
    batch
  }

  def batchAsRowIter(batch: ColumnarBatch): Iterator[InternalRow] = {
    CompletionIterator[InternalRow, Iterator[InternalRow]](
      batch.rowIterator().asScala,
      batch.close())
  }

  def fromBlazeIter(
      iterPtr: Long,
      context: TaskContext,
      metrics: MetricNode): Iterator[InternalRow] = {
    fromBlazeIterColumnar(iterPtr, context, metrics).flatMap(batchAsRowIter)
  }

  def fromBlazeIterColumnar(
      iterPtr: Long,
      context: TaskContext,
      metrics: MetricNode): Iterator[ColumnarBatch] = {
    val allocator =
      ArrowUtils2.rootAllocator.newChildAllocator("fromBLZIterator", 0, Long.MaxValue)
    val provider = new CDataDictionaryProvider()
    val nativeBlazeIterWrapper = new NativeBlazeIterWrapper(iterPtr)

    val root = tryWithResource(ArrowSchema.allocateNew(allocator)) { consumerSchema =>
      tryWithResource(ArrowArray.allocateNew(allocator)) { consumerArray =>
        val schemaPtr: Long = consumerSchema.memoryAddress
        val arrayPtr: Long = consumerArray.memoryAddress
        val hasNext = nativeBlazeIterWrapper.nextBatch(schemaPtr, arrayPtr)
        if (!hasNext) {
          return CompletionIterator[ColumnarBatch, Iterator[ColumnarBatch]](
            Iterator.empty, {
              JniBridge.updateMetrics(iterPtr, metrics)
              JniBridge.deallocIter(iterPtr)
              allocator.close()
            })
        }
        val root: VectorSchemaRoot =
          Data.importVectorSchemaRoot(allocator, consumerArray, consumerSchema, provider)
        root
      }
    }

    new Iterator[ColumnarBatch] {
      private var batch: ColumnarBatch = _
      private var finished = false

      context.addTaskCompletionListener[Unit] { _ =>
        finish()
        allocator.close()
      }

      override def hasNext: Boolean =
        !finished && {
          if (batch == null) { // first call
            batch = rootAsBatch(root)
            return true
          }
          tryWithResource(ArrowSchema.allocateNew(allocator)) { consumerSchema =>
            tryWithResource(ArrowArray.allocateNew(allocator)) { consumerArray =>
              val schemaPtr: Long = consumerSchema.memoryAddress
              val arrayPtr: Long = consumerArray.memoryAddress
              val hasNext = nativeBlazeIterWrapper.nextBatch(schemaPtr, arrayPtr)
              if (!hasNext) {
                finish()
                return false
              }

              Data.importIntoVectorSchemaRoot(allocator, consumerArray, root, provider)
              batch = rootAsBatch(root)
              true
            }
          }
        }

      override def next(): ColumnarBatch = batch

      private def finish(): Unit = {
        if (!finished) {
          finished = true
          JniBridge.updateMetrics(iterPtr, metrics)
          JniBridge.deallocIter(iterPtr)
          root.close()
        }
      }
    }
  }
}

class NativeBlazeIterWrapper(iterPtr: Long) {
  private val retQueue = new SynchronousQueue[Object]()
  private val errQueue = new SynchronousQueue[Throwable]()
  private var unfinished = true

  JniBridge.loadBatches(iterPtr, retQueue, errQueue)

  def nextBatch(schemaPtr: Long, arrayPtr: Long): Boolean = {
    assert(unfinished)
    putWithExceptionCheck(schemaPtr, arrayPtr)
    unfinished = unfinished && takeWithExceptionCheck()
    unfinished
  }

  private def putWithExceptionCheck(schemaPtr: Long, arrayPtr: Long): Unit = {
    while (!retQueue.offer((schemaPtr, arrayPtr), 1, TimeUnit.SECONDS)) {
      checkException()
    }
  }

  private def takeWithExceptionCheck(): Boolean = {
    while (true) {
      val ret = retQueue.poll(1, TimeUnit.SECONDS)
      if (ret != null) {
        return ret.asInstanceOf[Boolean]
      } else {
        checkException()
      }
    }
    false // unreachable
  }

  private def checkException(): Unit = {
    val maybeThrowable = errQueue.poll()
    if (maybeThrowable != null) {
      throw maybeThrowable
    }
  }
}
